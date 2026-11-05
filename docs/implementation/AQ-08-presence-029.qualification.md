# State29 scoped-presence prerequisite qualification

Frozen source `85160a7` passed11 distinct PG tests, strict server all-target/
all-feature Clippy and package format. The source inventory records15 exact
`git show` paths and was compared to current bytes before retention.

This registers actual state29 and qualifies the authenticated InventoryV2 library
owner and compatibility fences. It does not install new HTTP routes, qualify the
per-attempt report owner, consume those facts in renewal, or qualify physical
worker/provider execution. The [checkpoint scope](AQ-08-presence-029-checkpoint.md)
and [machine record](AQ-08-presence-029.qualification.json) retain those limits.

Rerun commands below use the leased slot target and disable incremental builds;
see [Cargo worker build guidance](../guides/cargo-worker-builds.md).
Historical commands, results and hashes remain in the machine-readable evidence.

```sh
env DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=3 CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-server --test scoped_presence_inventory --test schema29_presence --test schema28_closure --test native_inventory --all-features --locked --offline
env CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p aq-server --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --package aq-server --check
```

The tests include six actual inventory cases, two state29 bootstrap/upgrade-
rollback cases, two state28 regressions and one existing cold inventory case.
Private material files are actually opened by the bounded owner; no secret values
are returned. Root-owned exact0.1.9 package remains unchanged by this server slice.

The retained negative run found INSERT-trigger behavior preceding UPSERT's UPDATE:
an attempted restoration correctly failed its immutable epoch check. The owner
now uses locked UPDATE/INSERT, preserving the trigger rather than weakening it.
A nonminimal boolean lint was corrected without suppressions. Both development
failures and original raw log whitespace remain retained. Source and evidence
commits are separate; full root assembly is an independent gate.
