# Scoped presence owner qualification

Frozen source `7a19e0a0a77f7a2faceb4470c20f05d2ea1b2bcb` in `/tmp/aq08-installed-presence` adds machine report/inventory routes, the scoped renewal consumer and ordered state31. It follows integrated state30; public admission0.1.9 and migrations029/030 are unchanged. The JSON inventory hashes exact `git show` bytes from the frozen commit, rather than live files.

The focused combined run passed54 tests, failed0 and explicitly ignored5 artifact-dependent actual Git/worker cases. It includes8 actual scoped-report HTTP cases, existing grant/renewal regressions,6 inventory tests and schema28–31 upgrade/rollback cases. Strict server all-target/all-feature Clippy and package formatting passed. Final logs are retained with SHA256 inventories.

The fixture uses actual daemon offer/Accept/material/Grant/receipt owners and explicitly synthetic worker boot/read observations. It does not qualify physical worker reads, provider execution, simultaneous-attempt placement/storage load or overall native completion. Full-workspace qualification is left to root integration.

Commands:

Rerun commands below use the leased slot target and disable incremental builds;
see [Cargo worker build guidance](../../guides/cargo-worker-builds.md).
Historical commands, results and hashes remain in the machine-readable evidence.

```sh
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=2 cargo test -p aq-server --all-features --locked --offline --test native_initial_grant --test scoped_presence_inventory --test schema28_closure --test schema29_presence --test schema30_preparation --test schema31_presence_barrier
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p aq-server --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --package aq-server --check
```

Development negatives are retained separately: initial state31 omitted its SQL metadata update; the first combined state31 run exposed four state30 tests with stale final-version assertions; an invalid test-target invocation and the earlier RawSql/Send compiler diagnosis are not represented as passing qualification. Corrections are included in the frozen source and final checks pass.

See [checkpoint](../AQ-08-presence-owner-checkpoint.md) and [delivery plan](../AQ-08-scoped-material-presence-plan.md).
