# Pre-offer retained base export owner

This complements native migration022. It resolves the circularity in the legacy
base-provision route, which requires an attempt before exporting bytes needed to
seal that attempt's spec. The source-admission observation artifact keeps its
existing meaning; the exported bundle is a separate owned fact.

`profiled_execution::source_exports` owns the immutable request, quota row and
opaque ReservedExport token. Only a committed reservation can mint a production
token. The request pins operation/project/revision, repository/logical identity/
binding generation, exact nonzero SHA1 base, existing source retention handle and
source-observation ArtifactRef. It has no node, attempt, spec hash, path or URL.
Component tests have an explicitly gated synthetic token factory.

`source_retention::profiled_base` owns fixed private Git I/O and local verification.
It accepts the token, uses the configured bulk semaphore and bounded Unix socket,
and sends only artifact.export with source=base, no prerequisites, no reference-
only shortcut, and the original operation ID. The finite caller-correlation binds
the project/revision/source observation to this export. Timeout/unknown responses
keep the reservation and original request; no new operation is invented.

Verify the exact returned export handles/manifest, then open the configured Git
root/artifacts/UUID directory and manifest/bundle through no-follow directory/file
descriptors. Require operator-owned directories without other writers and regular
files; reject symlinks, devices/FIFOs and unsafe path components. Bound manifest
reads and stream bundle hashing in fixed chunks, matching exact file size and
SHA256, and detect file identity/size/timestamp changes during verification.
No request can choose a path. An exported base must be a full bundle with no
prerequisites, correct SHA1 source/base, producer installation and canonical Git
manifest fields matching both the response and disk bytes.

After verifying bytes, inspect the new transfer retention handle (operation ID),
not merely the original source handle. Require exact active generation/repository/
source pin with no release. The returned VerifiedExport carries original request
digest, bounded manifest bytes/digest, bundle size/digest, producer installation,
verified export response and retention inspection, with an original observed-time
window <=30s. These are actual owner observations, not a worker TransferManifest.

The022 caller persists an immutable verified fact, with request/digest/scope/size
checks, then fences current project/source/policy/runtime facts before offering.
A later retry reads the original immutable fact but freshly verifies retention
before new admission; it never extends the original observation by replaying its
receipt. Only after sealing the actual attempt/spec can the producer create the
existing attempt-bound TransferManifest wrapper. Worker download/import verifies
those digests independently.

Quota uses the existing719247031 source accounting lock. Reserve worst-case
bundle+manifest bytes before I/O, account unknown/orphaned exports, and never
silently prune uncertain work. Legacy transfers/bases and new exports share one
aggregate budget. Attaching an attempt wrapper cannot charge another reservation
for the same owned export; release needs its own qualified retained-owner path.
No ordered migration qualification is claimed before022 and prior migrations
exist together.

Tests separately cover fixed socket scope/replay, changed returned binding,
missing/changed manifest or bundle, digest/size mismatch, symlink/nonregular and
concurrent file replacement, inactive/released retention, bounded reads, current
observation time, quota/token boundaries, and actual disposable aq-git export.
Synthetic socket/filesystem fixtures do not substitute for actual Git qualification.
