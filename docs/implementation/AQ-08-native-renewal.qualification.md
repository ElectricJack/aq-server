# AQ-08 native continuing-authority checkpoint

Frozen source: `c5438d3` in `feat/aq08-native-renewal`. This is a component
checkpoint, not AQ-08 completion or real-agent execution qualification.

The actual daemon path now accepts bounded authenticated
`POST /v1/worker/profiled/renew` requests using frozen admission0.1.7. It renews
one still-live grant under fresh current owner facts, retaining the original
choice, input pins, owner, grant ID and immutable hard deadline. It never calls
the selector or a model provider. New renewal receipts and hot authority commit
atomically. Exact replay freshly stamps only its original still-current permit;
a changed, superseded or expired operation refuses. The schema28 closure fence
is mandatory for both new authority and replay.

The positive SQL/HTTP fixture starts with the actual authenticated inventory
owner reporting no installed credentials, producing daemon-owned provisionability
proofs. Actual ready/context, composition, selection, source and artifact owners
publish an offer; actual Accept, Prepare and retained installation precede Grant
and Renew. A second renewal succeeds after the original admission/material
retrieval clock expires while original decision/offer/grant JSON remains equal.
Other cases refuse a newer unavailable report, missing retained installation,
changed private bytes, revoked agent scope, expired permit and changed/superseded
operation content. Qualification claims remain explicitly synthetic.

## Checks

The frozen source passed 53 tests: 25 native initial-grant/renewal SQL/HTTP tests,
2 schema28 migration tests, and 26 focused profiled-execution library tests.
Two pre-existing tests requiring an actual Git peer were explicitly ignored;
this checkpoint makes no new actual-Git qualification claim. The six new HTTP
renewal tests are included in the 25, not counted twice. Library tests include
private read progress despite a saturated Tokio blocking pool, cancellation
ownership bounds, scoped sequence-first health and causal installation rules.

Rerun the focused selection from the leased slot root:

Rerun commands below use the leased slot target and disable incremental builds;
see [Cargo worker build guidance](../guides/cargo-worker-builds.md).
Historical commands, results and hashes remain in the machine-readable evidence.

```sh
env DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=4 CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-server --test native_initial_grant --test schema28_closure --all-features --locked --offline
env CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-server --lib profiled_execution:: --all-features --locked --offline
env CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p aq-server --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all --check
```

The final integration tests took 40.89s and 0.45s respectively; the focused
library tests took 0.01s. Strict lint completed in 52.41s. Compile time is recorded
separately in the retained logs. These timings are observations, not latency or
fleet capacity acceptance.

The [machine-readable inventory](AQ-08-native-renewal.qualification.json) hashes
retained logs and a `git show c5438d3:path` source inventory: 21 changed paths plus
8 direct prerequisite/build paths. Git commit identity pins the complete tree.
No public DTO, package archive, Cargo dependency or migration was changed by
`c5438d3`; prerequisite schema28 and strict initialGrant fencing were imported
from their owners before the final checks.

## Boundaries and retained negative evidence

Renewal has an independent eight-request admission lane, two PostgreSQL
connections, two private-read threads, two queued jobs and four owned jobs.
Cancelled waiters retain job ownership through actual read and zeroization.
Private material reads observe current bytes before final SQL fences; filesystem
and database state are not an atomic snapshot. No live authorization is cached.
Cold immutable verification and finite history quotas remain bounded work, with
no throughput claim.

The inventory endpoint currently rejects bare installed=true reports for
server-material credentials and carries no typed installation receipt input.
Thus first-profile continuation is bounded by the original inventory expiry;
a newer missing/preparation-only report correctly denies renewal until a genuine
receipt-bound installed-inventory owner exists. Material retrieval expiry itself
does not terminate an already installed unchanged credential binding.

Additional adversarial qualification remains for quota/late-lock rollback,
closure/cancellation races and delayed Started after renewal. Worker permit
maintenance, receipt-bound periodic inventory, real configured runtime/model/
counting evidence, cold preparation phase separation and full root assembly are
separate gates. Historical Reconcile remains owned by its separate service.

Development failures are retained alongside final passes: the first renewal
matrix found a PostgreSQL int4-to-i64 quota decode mismatch, corrected by an
explicit BIGINT cast. A manually altered cold fixture lacked actual
provisionability proof and was correctly refused. Its replacement used the real
inventory owner; a stale poll inventory sequence was then correctly refused and
fixed to the acknowledged sequence2. These fixture/decode failures are not
omitted or counted as successful cold-path qualification. Temporary diagnostic
logging was removed before the final source freeze.
