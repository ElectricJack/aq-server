# Scoped head and controlled-capacity qualification

Frozen source `b71bdf05cbaf62b6f7071183bf107d6a8e70ae6c` from root state31 `c3f2df1`, in `/tmp/aq08-presence-isolation`. Exact `git show` source hashes and retained log hashes appear in qualification.json. Four actual HTTP/storage cases passed in6.44s; three schema31 cases passed in0.97s. Strict server all-target/all-feature Clippy and package formatting passed.

The first command filters the native test to4 isolation cases and also invokes schema31 with0 selected cases. The separate unfiltered schema31 command supplies all3 schema cases; no zero-selected run is counted as schema qualification.

```sh
CARGO_TARGET_DIR=/tmp/aq08-native-producer-target CARGO_BUILD_JOBS=2 DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=2 cargo test -p aq-server --all-features --locked --offline --test native_initial_grant published::presence::isolation --test schema31_presence_barrier
CARGO_TARGET_DIR=/tmp/aq08-native-producer-target CARGO_BUILD_JOBS=2 DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=2 cargo test -p aq-server --all-features --locked --offline --test schema31_presence_barrier
CARGO_TARGET_DIR=/tmp/aq08-native-producer-target CARGO_BUILD_JOBS=2 cargo clippy -p aq-server --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --package aq-server --check
```

B's scope is explicitly synthetic relational storage with no runnable offer/claim/hot authority. Controlled request-body barriers prove admission saturation; held PG connections prove the renewal pool remains independent. These are deterministic isolation checks, not public multi-slot placement or sustained fleet/load evidence. No provider calls occur. The only production delta is fail-closed Store reopening when current31 binding-set storage is missing; no silent repair or comprehensive schema attestation is claimed.

See [checkpoint](../AQ-08-presence-isolation-checkpoint.md).
