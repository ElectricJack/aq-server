# Closure schema and launch-open prerequisite

Source checkpoint: `75015d0`, based on `53a24d1` plus Reconcile equivalents
`cbf9717` and `185d4a8`. This is an ordered-schema/history/fence prerequisite,
not an activated closure owner or a qualified worker proof producer.

Migration 028 adds bounded immutable semantic registration history, per-attempt
original registration links and terminal launch-closure history. Fresh startup
and actual SQL prefix 27 upgrade/rollback/refusal are exercised. Default worker
closure POST remains 404. Registration and acceptance capture actual admitted
metadata; a missing old-boot provenance record is refused rather than inferred
from a replacement boot. Exact registration retries do not treat mutable
operation/configuration/time refreshes as new semantic identity.

`closure::ensure_launch_open_locked` is strict: missing schema is an error. Its
caller must already hold current node and exact attempt/hot locks. The Started
owner invokes it; initial Grant and Renew owners consume the same prerequisite
in their separate source slices. Physical Prepared/Started journal checkpoints
remain distinct from acknowledged renewal counters.

The first actual-offer smoke failed because PostgreSQL `sum(BIGINT)` returns
NUMERIC, while the link quota reader requested i64. The reader now explicitly
casts its bounded sum to BIGINT. The negative log is retained. The corrected
run passed four actual publication/Accept cases and two schema cases. A final
SQL-only nullable-kind hardening rerun passed both schema cases (41.68 seconds
build, 0.45 seconds tests). No performance or saturated-start claim is made.

Commands used the disposable pinned PostgreSQL on port 55432 and:
`RUST_TEST_THREADS=2 CARGO_TARGET_DIR=/home/jkern/dev/agent-q/target
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-server --locked --offline`
with `--test native_offers --test schema28_closure` for the six-case run, then
`--test schema28_closure` for final two cases. Schema unit compatibility was
also compile-checked separately. Full owner lint/race/auth qualification remains
pending; subsequent service/policy drafts are not attributed to this checkpoint.

History caps are 4096 rows/64 MiB global and 1024 rows/16 MiB per node.
Attempt links have separate 4096-row/262144-byte global and 256-row/16384-byte
per-node logical UUID-payload caps. Closure history has 4096 rows/64 MiB global,
256 rows/4 MiB per node, counting actual retained bytea and PostgreSQL JSONB text
bytes for binding/verifier metadata. Physical SQL row/index overhead is separate.
No expiry, task claim, source retention or effect settlement is released here.

Exact source hashes and raw logs are retained in
[evidence/closure28-prerequisite-75015d0](evidence/closure28-prerequisite-75015d0).

For reruns, use the leased slot target and current dev/test profile from
[Cargo worker build guidance](../guides/cargo-worker-builds.md), with
`CARGO_INCREMENTAL=0`. The paths and debug overrides above record historical
runs; the original evidence is retained.
