# Schema37 reservation fixture seeding repair

Task: `eager-grove-45.3.3`. Source base: `c13cf4bf1ba1aa382af147ac9e502e1017bc7473`.

The reservation fixture deliberately opens the registered migration prefix 37.
Its HTTP enqueue seed started requiring the dashboard operation journal added in
migration 42 after the handler was routed through dashboard action admission.
The historical fixture therefore failed before reaching its reservation checks.

The fixture now uses a `cfg(test)` adapter around the existing `enqueue_domain`
writer. The adapter retains real request validation, current-owner authorization,
mailbox append, retention accounting, replay and immutable receipt persistence.
Production HTTP enqueue and dashboard action admission are unchanged. The
original schema37 assertion and all reservation assertions remain; every seed
also checks that the actual version is still 37 and `dashboard_operations` is
absent. No migration or schema relabeling is performed.

## Reproduction

Use the synthetic disposable PostgreSQL service from `scripts/test-db`:

```sh
export DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres
export CARGO_TARGET_DIR="$PWD/target"
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
cargo test --locked -p aq-server --lib agent_access::messaging::global::reservations::tests -- --test-threads=2
cargo test --locked -p aq-server --test supervisor_conversation --test dashboard_actions -- --test-threads=2
cargo clippy --locked -p aq-server --lib --tests -- -D warnings
cargo fmt -p aq-server --check
git diff --check
```

`aq test --aq-help` confirms that the available runner supports pytest and
Bazel, not Cargo. These task-required Rust checks use foreground Cargo with two
build jobs, two test threads and no incremental compilation. Run from the leased
slot root and reuse its own target. The workspace dev/test profile retains
file:line backtraces with line tables; do not override it with debug=0 for reruns.
See [Cargo worker build guidance](../../guides/cargo-worker-builds.md).

## Validation evidence

The required reservation module run passed all 12 tests (nine database cases,
three contract/schema cases), with 0 failures and 0 ignored tests. Full output
is retained in `reservations.log`. The post-sync run is retained separately in
`reservations-after-sync.log` and also passed all 12 tests with 0 failures and
0 ignored tests.

The current-schema regression targets passed all 8 dashboard action tests and
all 6 supervisor conversation HTTP tests, with 0 failures and 0 ignored tests.
This includes action/legacy enqueue deduplication, retained dashboard operation
proof, replay/reconciliation, authorization, CSRF, retention and input bounds.
Full output is retained in `current-routes.log`.

Strict `cargo clippy --locked -p aq-server --lib --tests -- -D warnings`
passed (exit 0); full output is retained in `clippy.log`.

`cargo fmt -p aq-server --check`, direct rustfmt for both changed files, and
`git diff --check` passed after the final sync. No full workspace test run was
performed for this focused repair.

`cargo fmt --all --check` is unavailable in this nested worktree: Cargo metadata
resolves the excluded vendored `aq-lifecycle-protocol-0.1.0` against the enclosing
main checkout's workspace and rejects its membership. The full diagnostic is
retained in `cargo-fmt.log`. The scoped `cargo fmt -p aq-server --check` and
direct rustfmt checks for both changed files pass.

The project supervisor waived the generic `origin/dev` final-sync instruction
in message `msg-c487fcf5329a4646a4b1692c7489fc87`: this repository has no `dev`
branch and its integration train is off. The required final sync is a fetch and
merge of `origin/main` into the assigned task branch; publication remains on
`aq/eager-grove-45.3.3`.

`aq git pull --branch main` fetched and merged
`0254aac7f9472f4bffe138c0fe39c139a0682b0a`; the explicit subsequent
`git merge origin/main` reported already up to date. This sync changed only
three documentation/task files. The original implementation commit is retained
separately from the merge ancestry.
