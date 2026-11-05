# AQ-08 renewal adversarial followup

Test source `202b9f8`; production source remains `c5438d3`. Four focused actual
SQL/HTTP cases passed, strict target lint and server format passed. This adds four
distinct tests to the earlier checkpoint's 53; it does not repeat or replace the
full root assembly gate.

The cases exercise actual assembled offers and grants: a receipt-trigger failure
rolls back hot authority and permits same-operation retry; an observed final quota
lock wait crosses the old permit deadline and rolls back both extension and
receipt; changed authorized root CAS/content refuses; delayed Started retains its
actual original physical checkpoint even after a newer renewal counter was
acknowledged. Exact renewal replay after Started preserves original receipt
versions and expiry. Further new renewal must exceed the maximum physical/renew
counter and cannot overwrite a physical checkpoint digest.

The Started body uses an explicitly synthetic qualified host fixture. This proves
server ordering and ownership, not physical execution or provider qualification.
No production code, DTO, package or migration changed in this followup.

Rerun commands below use the leased slot target and disable incremental builds;
see [Cargo worker build guidance](../guides/cargo-worker-builds.md).
Historical commands, results and hashes remain in the machine-readable evidence.

```sh
env DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=2 CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-server --test native_initial_grant published::renewal::races --all-features --locked --offline
env CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p aq-server --test native_initial_grant --all-features --locked --offline -- -D warnings
cargo fmt --package aq-server --check
```

The tests took 6.71s. The [machine-readable record](AQ-08-native-renewal-races.qualification.json)
hashes original logs and both test files from `git show 202b9f8:path`. Literal log
whitespace is retained. Source and evidence commits remain separate.
