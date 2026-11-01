# Human result media, version 1 optional port

Closed optional human interface. Configuration is absent by default and upload
mutations additionally require explicit opt-in. Source/router qualification and
an isolated actual browser gate precede canonical enablement.
See [design](../../../docs/specs/result-media-png.md) and
[implementation delta](../../../docs/implementation/API-11-png-first-plan.md).
The example contains labelled synthetic contract values, not retained PNG proof.

`schema.json` defines closed UploadCreate, Empty, UploadStatus, MediaPage and
ContentFailure objects. Responses identify `human-result-media-1`. Report revision
is the existing bare lowercase SHA256; new byte/receipt/request digests have the
explicit `sha256:` prefix. UUIDs are lowercase and nonnil. All unknown query/body
fields and duplicate JSON keys are rejected. Filenames/scenario/environment
strings also have UTF8 byte bounds and no controls. Filenames additionally reject
path separators; capture labels are never interpreted as paths. Schema lengths
alone do not establish these semantic checks.

## Routes and bodies

Let R be `/api/human/results/{report}/revisions/{revision}`.

| Method/path | Request and result |
| --- | --- |
| POST `R/media-uploads` | UploadCreate, at most 16KiB; 201 newly reserved or 200 exact replay UploadStatus |
| GET `/api/human/result-media-uploads/{operation}` | No query/body; pure 200 UploadStatus, original actor only |
| PUT `/api/human/result-media-uploads/{operation}/content` | Whole image/png matching retained size/SHA; 200 received/retained, 202 busy, 410 expired/422 rejected UploadStatus |
| POST `/api/human/result-media-uploads/{operation}/finalize` | Exactly `{}`; 200 retained, 202 processing, 409 input needed, 410 expired/422 rejected UploadStatus |
| GET `R/media` | No query/body; 200 MediaPage, at most eight immutable receipts/tombstones |
| GET `R/media/{artifact}/content?variant=preview\|render` | Exactly one recognized variant; 200 image/png after final role/link/expiry/digest fence |

All routes require current human Owner. Admission/content/finalize require the
upload capability, while finalized media browse/content requires the read
capability. Actor identity is obtained from AQ authentication, not the request.
Original-actor status remains readable when upload mutations are disabled; lookup
never retries a write or silently extends an original deadline.
Machine credentials are not accepted. Unknown/foreign actor intent returns 404;
generic malformed/auth/conflicting-operation responses use AQ `{error,message}`
without a previous actor's private fields. A404 does not authorize a changed
submission after an in-flight admission.

Known authorized media content failures are closed ContentFailure bodies:
410 media_expired/media_withheld, 404 media_missing, 409 media_failed. Unknown or
unauthorized object associations disclose no descriptor; 401/403 are application
auth failures, not a missing-image placeholder. No original-input, download,
arbitrary URL/path, Range or conditional-cache surface is provided. Reject
unsupported Range/conditional headers after authorization with 416/400.

On success Content-Type is image/png; Content-Length matches the chosen variant;
AQ-Media-Digest and AQ-Media-Receipt-Digest match the variant and immutable
receipt. Send Cache-Control: private, no-store; X-Content-Type-Options: nosniff;
Content-Disposition: inline; Content-Security-Policy: default-src 'none'. A
browser client validates the bound receipt/variant/digest and never substitutes
an arbitrary response URL. HTTP routes and headers are mediated by AQ.

## Required cross-field semantics

- Operation UUID equals the retained artifact UUID. Scope/source in every receipt
  matches the actual report owner at the exact URL digest. Current/head status
  does not relabel an old receipt, and media presence does not mean acceptance.
- Request digest hashes the literal accepted admission JSON bytes. Reuse with
  changed bytes conflicts, including changed whitespace. Input SHA/size is fixed;
  raw PUT retries must be identical. Finalize accepts no replacement content.
- Capture fields are optional human assertions. The server binding is distinct
  from claimed capture source/time/environment; never present a matching claim
  as verified revision coverage or worker capture.
- A retained state has one immutable artifact/receipt and cannot regain write
  recovery. Other states have no finalized artifact. Rejected/expired states
  cannot renew/replay. Finalizing permits no new physical writer while its job
  remains live. GET may report stale pending; it never reconciles files.
- Reserved normally permits PUT only (input_required); received permits original
  PUT/finalize; finalizing is busy until its physical job ends, then can permit
  explicit finalize reconciliation within the original budget. Retained permits
  no write recovery, but an exact PUT replay may return its existing status after
  validating the submitted bytes without writing. No changed bytes are accepted.
- Timestamps are positive JavaScript-safe millisecond integers, ordered relative
  to the original pinned deadlines. Clock regression blocks writes. Capture time
  is an assertion and does not extend server authority/retention.
- Canonical receipt bytes are UTF8 JSON with recursively sorted object keys,
  compact separators and no floating-point values. receipt_digest hashes exactly
  that Receipt object, excluding dynamic availability and its own digest.
- Render and preview are static RGBA8 PNG under the pinned rendering policy.
  Render dimensions match the actual accepted input; checked width×height is at
  most 2,000,000. Preview uses floor(width×scale), floor(height×scale), minimum 1,
  scale=min(1,384/max(width,height)), with deterministic nearest samples. No
  upscale, implicit crop, EXIF/color profile, second frame or source replacement.
  Final encoder settings are perceptual sRGB, Compression::Fast then
  Filter::Sub, pinned dependency bytes, and no copied metadata. Source sample
  selection for preview pixel(x,y) is floor(x×source_width/preview_width),
  floor(y×source_height/preview_height), with checked integer arithmetic.
  The pinned Fast encoder's stored/no-filter size fallback is part of the policy;
  both stored-small and actually-Sub-filtered vectors have golden digests.
- All item bindings equal MediaPage.binding, artifact IDs are unique and sorted
  by stable UUID. This is an attachment list, not chronological result Latest.
- Availability reason is null only for available. Other states have their
  matching typed reason. Available means owned files are present at expected
  lengths at observation; actual served digests are verified again on content
  read. Missing/corrupt/expired/withheld content is never a successful empty PNG.
- Limits may be lowered by server configuration; clients enforce the returned
  bound and the schema's hard maxima. Every byte/file/metadata reservation is
  accounted before writing, including pending/orphan staging until confirmed
  cleanup. No lookup or repeated finalize silently extends a deadline.
  UploadStatus.limits is the original pinned admission policy; MediaPage.limits
  is current retrieval policy. Lowering configuration cannot rewrite an existing
  intent's deadlines or immutable receipt. Unsupported native-report coverage
  is rejected; every binding explicitly declares recorded_legacy_native_only.

## Storage and migration review

`proposed-ddl.sql` remains an earlier review sketch outside migrations; it is not
an installation source. Actual migration 36 and its startup refusal guards live
in `crates/aq-server/migrations/0036_result_media_owner.sql` and
`store/media36.rs`. See the actual
[storage qualification](../../../docs/implementation/API-11-storage-owner.md)
and [optional HTTP port](../../../docs/implementation/API-11-http-port.md).
No source qualification authorizes executing a migration on a canonical database.
