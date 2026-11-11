# Swarm concurrency controls

Implements the 2026-11-11 revision of spec 0007: owner-managed Normal and optional Reduced usage concurrency matrices, with stable intelligence profile IDs and configured names. Task routing chooses requirements independently; admission waits for a matching provider/class slot. Automatic routing has its own catalog bucket. An unsaved Normal matrix preserves existing behavior; limits are never guessed.

The owner API is `GET/POST /api/settings/swarm`, `POST /api/settings/swarm/mode`, and `GET /api/settings/swarm/operations/{id}`. Writes fence settings/catalog revisions, atomically save coherent matrices, and retain actor-scoped idempotent operation receipts. Zero pauses new starts. Reduced values cannot exceed Normal values.

Native offers, manual preparation and supervisor generations reserve shared capacity under one database lock before starting. Existing reservations survive lowered limits. Authoritative stop/failure evidence releases occupancy. Saturated tasks stay queued while the native poll checks other work; a pending asynchronous poll may return empty before producing its eventual offer. Waiting observations expire and are reclaimed in bounded indexed batches. Historical occupancy with no provable class is visible and conservatively blocks affected new starts. Active counts include reserved starts; waiting counts cover pending manual agents and recent slot refusals, rather than the entire unclassified external graph.

Migration 0115 and the supervisor-host binary must roll out together before configuring limits. The database rejects unreserved generations from older hosts after limits exist. Existing generations may finish; upgrade/restart hosts after active turns exit. See `crates/aq-supervisor-host/README.md`. No aq-worker upgrade is needed for the added manual metadata. Host terminal shells and deterministic test-only harnesses are outside provider-model ceilings.

The additive `routing_profile` Swarm field reports each attempt's pinned identity/name. Its legacy fast/standard/deep classification remains a palette hint. The companion dashboard pins this server contract and supplies Settings › Swarm, the Reduced usage control and a persistent collapsible header. Cloud automation and additional per-machine resource policy remain separate phases.

To meet the existing build ratchet, unchanged bounded human-read admission moved to aq-human-http, and release-manifest validation moved to aq-dashboard-contract. Server imports remain compatible. The server library shrank from 108915 to 108849 lines; integration-test binaries remain 81.

Validation: 33 focused Rust tests passed, including 7 settings HTTP cases, 3 native cases, 3 manual/admission cases, 13 Swarm regressions, 2 supervisor integration/plan cases and 5 asset/read-gate regressions. Changed-package Clippy with all targets and warnings denied, formatting, migration catalog and build-budget checks passed. Protocol fixtures validate the additive schema. The original node-lock regression now waits for the documented asynchronous offer before asserting the unchanged lock behavior.

## Owner preview deployment, 2026-11-11

Deployed server code `939ee9a4` with migration 115, the official clean dashboard `6568e6eb`, and a Debian-compatible supervisor host/helper built from the same server source. The qualified Codex closure and retained state/workspace volumes were preserved. Both enabled supervisor controllers report idle on the new image. New containers receive a 30-second stop timeout so shutdown can collect the required stopped-writer proof.

The exact official dashboard manifest (`1dfb8c41d0a91fb87f907537c876ae677fa2e9f4889deeded665fc337fd47b05`) and all 64 served assets match the staged bundle. Server health, schema, source revision, Graph and Git readiness passed. Two existing owner agent records retain running status, and all four existing worker/terminal service PIDs are unchanged. Normal and Reduced matrices remain unset, with Normal mode active, for owner configuration.

Live desktop/phone browser checks passed with no accessibility violations or page overflow. Evidence remains local under `/home/jkern/dev/aq-platform/qa/2026-11-11/swarm-controls-rollout`. Private rollback snapshots and receipts are under `~/.local/share/aq-ms1/swarm-controls-rollout-20261111T193538Z/`; they contain private state and must not be published. Schema 115 requires its compatible server; preserve subsequent writes and fix forward instead of restoring an old snapshot over them.

Broader server CI remains affected by baseline failures: all 47 failing test names also fail on parent `8dca8043`. The focused feature checks and live preview passed. This deployment does not establish that the full repository CI suite is green.
