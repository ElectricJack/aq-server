# Duplicate interactive DeliveryClock repair

Task: `eager-grove-45.3.1`. Source base: `ab314605eeccf89bb49cc2d23e4fc6e98c230bd7`.

Two independent archived-transport repairs (`2a4e6ea8` and `d2a02851`) moved
the same three inherent `DeliveryClock` methods into different private modules.
Compiling both modules produced E0592/E0034. Remove `interactive_delivery.rs`
and its module registration; retain `delivery_clock_interactive.rs`, including
its request/response interval documentation. Correct its comment spacing.
Public method paths, validation errors, one-hour interactive bound and checked
deadline arithmetic retain their existing behavior.

The frozen `src/transport.rs` is unchanged. Its SHA-256 remains
`4f19c6c0bb728e781fb6349c4a994e28c31b0ff665fe11628c70169df9ceea50`.
Schemas, fixtures, archived inventories and existing tests are unchanged.

## Validation

Cargo runs through a temporary pytest subprocess adapter admitted by
`aq test .aq/tmp/test_delivery_clock_validation.py`, with AQ's default worker
cap and marker exclusions. The adapter runs Cargo commands sequentially with
`CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0`.

- `cargo test --locked -p aq-admission`: 99 tests pass; zero failures or
  ignored tests. Includes frozen source/publication preservation, interactive
  invalid timestamp and overflow rejection, one-millisecond and one-hour
  bounds, and original transport TTL/projection coverage. See `admission.log`.
- `cargo test --locked -p aq-server --test source_retention --test
  project_bindings --no-run`: passes; both server integration test executables
  are built. See `server-compilation.log`. This checks compilation; it does
  not run the database-backed server tests or qualify Docker/aq-git behavior.
- `rustfmt --edition 2024 --check --config skip_children=true
  crates/aq-admission/src/lib.rs
  crates/aq-admission/src/delivery_clock_interactive.rs`: passes.
- `git diff --check`: passes.

## Final synchronization

The generic instruction's `git merge origin/dev` cannot resolve that absent
ref. The repository's default is `origin/main`. `aq git pull --branch main`
and `git merge origin/main` succeed; the latter reports already up to date at
the source base above. Synchronization changes no source files.

After synchronization, reran only the touched area's checks under `aq test`:
`cargo test --locked -p aq-admission --test preparation_refresh --test transport`
and the same server `--no-run` command. All 19 targeted tests pass, and both
server integration test executables compile. Results are retained separately in
`admission-after-sync.log` and `server-compilation-after-sync.log`.
