# aq-result-media pure core

Unpublished Rust component for the PNG-first supplemental media contract. It
validates closed domain data and produces deterministic static PNG variants.
It has no SQL, filesystem storage, HTTP, Graph, worker or authentication client.
A valid binding/receipt DTO is not proof of authorized retention; only the
daemon's actual retained legacy result and media storage owners can establish it.
Unsupported native coverage cannot decode as a successful empty media page.

The first codec accepts up to 1 MiB, 2,000,000 pixels and 4096px per axis; static
noninterlaced RGB/RGBA8 and the narrow documented chunk set only. It preserves
actual sample/alpha bytes, expands RGB in place, and produces separately pinned
render/preview descriptors. The source is never silently substituted by a
normalized variant. Caller output and encoded writers have hard length checks;
the decoder's 16 MiB scratch setting is an additional best-effort defense and is
not a process RSS guarantee. A separately pinned, already-transitive
flate2 1.1.10 validates exact inflated scanline length, actual stream end and full
compressed consumption before pixel decoding, using at most a 1 MiB concatenated
IDAT copy and fixed 8 KiB scratch. Surplus/concatenated zlib data is rejected.

The workspace pins official png 0.18.1 plus exactly upstream malformed-PLTE
fix 8330c2c6fc91c56fb8c4bb0cd6ea65994027f15e in
`vendor/png-0.18.1-aq1`. Original archive/patch/patched-file hashes and licenses are
retained there. The direct-decoder regression covers the actual upstream failure;
public preflight rejects palettes before decoder invocation independently.

This crate is `publish=false`. A root workspace patch does not propagate to a
future published consumer. Publishing requires a qualified fixed upstream
release or explicit downstream vendoring/patch instructions and separate
package-consumer qualification; none is claimed here.

Run only this package's tests/clippy when the shared build slot is assigned.
The optional `qualify_peak` example reports a fresh process's Linux high-water
mark for a maximum-raster fixture; it does not establish hard allocation limits,
throughput at agent scale or media storage/retention/authorization qualification.
The final core passes 24 focused tests (codec group 1.20s), strict clippy and
formatting. A maximum-raster, near-input-limit diagnostic processed five images
in 3047ms in the unoptimized debug profile and observed a 14,428KiB process
high-water mark. This includes the compressed copy and fixture generation;
it does not establish a hard RSS bound or production throughput. See
`docs/implementation/API-11-core-qualification.md` for exact measurements,
separate encoded bounds and pinned golden vectors.

The server owner, routes, migration 36 and Results reader/expand UI are separate
followups. No actual media retention or human/worker authorization is qualified
by these pure tests.
