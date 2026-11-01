# Qualified pure PNG core, before media storage

Source `a3b2ead` adds unpublished `crates/aq-result-media`. Its scope is pure
closed-domain validation and deterministic PNG processing. It contains no SQL,
storage filesystem, HTTP, Graph or worker client and mints no authorization.
No migration 36 was registered/applied, no result/media capability was mounted,
and no dashboard media or upload UI is claimed by this delivery.

The exact official png 0.18.1 archive checksum is
`60769b8b31b2a9f263dae2776c37b1b28ae246943cf719eb6946a1db05128a61`.
Only the five-line upstream malformed-PLTE fix
`8330c2c6fc91c56fb8c4bb0cd6ea65994027f15e` changes original source files.
Original licenses, all 39 original-file hashes, patch bytes and patched-file
hashes are retained in `vendor/png-0.18.1-aq1/AQ-PROVENANCE.json`. Existing frozen
AQ vendor archives are untouched. The direct decoder regression exercises the
actual upstream malformed palette; public structural preflight independently
refuses palettes before invoking a decoder.

The pure package passed **24 focused tests**: 13 codec cases in 1.20s, 10 domain
cases and one buffer/CRC unit case. Strict affected clippy passed in 0.88s;
formatting and Draft202012 schema/example validation passed. Retained
[test and lint logs](evidence/api11-media-core/) identify the exact command scope.
No server library, database, provider or browser build was needed.

Meaningful cases include exact RGBA/alpha preservation and in-place RGB expansion,
maximum 2 MP buffers, deterministic preview coordinates, CRC/truncation/order/APNG/
profile rejection, malformed deflate/Adler, split single-byte IDAT chunks,
surplus raster and concatenated/trailing zlib streams, bounded mutation corpus,
exact stored/filtered goldens, required nullable fields, positional-object bypass,
raw intent digest versus typed equivalence, receipt/scope/availability/limit
mismatches, terminal replay restrictions and unsupported native coverage.

The high-level PNG reader intentionally discards surplus image rows. To enforce
this contract's stricter input policy, the core also pins its already-transitive
flate2 1.1.10 and validates exact `(width*channels+1)*height` scanline length,
actual StreamEnd and full compressed consumption. It uses a bounded 1 MiB IDAT
concatenation plus fixed 8 KiB scratch and aborts at expected+1. Decoder limits
do not replace the separately checked 8,000,000-byte full RGBA, 589,824-byte preview
RGBA, 8 MiB render and 768 KiB preview encoded bounds.

Before the first policy freeze, fixed Sub filtering replaced NoFilter following
the actual diagnostic. Both use the same pinned Fast encoder and unchanged
buffers/caps. Its deterministic stored/no-filter size fallback remains part of
the pinned policy; separate golden vectors cover both paths.

| Same maximum-raster diagnostic | NoFilter baseline | Qualified fixed Sub |
| --- | ---: | ---: |
| Input / concatenated IDAT |816,097 /816,040 bytes |816,097 /816,040 bytes |
| Render bytes |8,001,691 |1,135,568 |
| Preview bytes |295,205 |44,594 |
| Five iterations, unoptimized dev profile |4134ms |3047ms |
| Observed fresh-process VmHWM |33,032KiB |14,428KiB |

The render is 7.05× smaller and preview 6.62× smaller on this fixture. The process
measurement includes original input, compressed copy, pixel work, encoding and
fixture generation; it is an **observation, not a hard RSS bound or production
throughput guarantee**. No allocation cap was relaxed. Raw measurements and
their hashes are in [qualification.json](evidence/api11-media-core/qualification.json).

Every media binding now explicitly declares `recorded_legacy_native_only`.
Unsupported native coverage or a missing coverage field is rejected even for an
empty list; this component cannot turn an unsupported native report into an
available image or a successful empty report. The future daemon still must
resolve the actual legacy retained result owner, storage grant and current human
authority before publishing any descriptor or bytes. Well-formed DTOs alone are
not retained-media proof.

This package is `publish=false`. A root workspace patch does not propagate into
a separately published dependency consumer. Standalone publication requires a
qualified upstream fixed release or explicit downstream patched-source packaging
and consumer checks; neither is claimed here. Actual storage, quota/cleanup
races, response-loss IO recovery, human/worker scope and media UI remain the next
API-11/UI-18 slices under their separate review and build leases.
