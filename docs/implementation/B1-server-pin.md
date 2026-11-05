# B1 server pin: delegated sandbox admission 0.2.0

The server consumes aq-worker's owned `protocol/aq-admission` source release as
`aq-admission-sandbox =0.2.0`. The installed endpoints continue to use the local
`aq-admission =0.1.11` dependency. Permit issuance and enforcement belong to B2;
this change establishes the contract dependency and consumer conformance.

## Exact source

- Repository: https://github.com/ElectricJack/aq-worker
- Source revision: `c413cfe21fa0b8272b1f2bc2f1579669e6ad0f6c`
- Owner evidence: [B1.md](https://github.com/ElectricJack/aq-worker/blob/c413cfe21fa0b8272b1f2bc2f1579669e6ad0f6c/docs/implementation/B1.md)
- Source publication: `protocol/releases/aq-admission-0.2.0.tar.gz`
- [Pinned archive](../../vendor/aq-admission-0.2.0.tar.gz), SHA-256:
  `3c5054ebee1ecd87fc3380292830e650efcdd4d0beb43a779fd0cf9cd2eb64d7`
- [Source inventory](../../vendor/admission-contract.json) records the immutable
  download URL, revision, archive checksum and SHA-256 of all 119 extracted files.
- [Extracted package](../../vendor/aq-admission-0.2.0/) is byte-for-byte identical
  to that archive, including `artifacts/sandbox-v1.fixtures.json` and its schema.
  No sibling checkout, floating Git reference or registry publication is needed.

Cargo excludes the vendored package from workspace membership and pins it by
exact version and repository-local path. A crates.io patch uses the already
vendored lifecycle 0.1.0 contract, alongside the existing session 0.1.0 patch,
so the published package's dependency closure stays local and unchanged.
`Cargo.lock` records both admission versions; existing consumers retain 0.1.11.

## Consumer coverage

`crates/aq-server/tests/permit_fixture_conformance.rs` executes the owner's literal
expected outcomes for all 44 vectors: 15 permit, 5 re-adoption, 4 replay,
3 compatibility, 3 registration, 4 phase handoff and 10 runner renewal cases.
There are 12 accepted and 32 refused cases, including exact replay, changed
request/response replay, expiry, wrong boot/attempt/generation/channel/sandbox,
preparation scope, hard deadlines and unsupported required capabilities.
The test asserts the release inventory and generated sandbox schema, and verifies
the archive plus every extracted file against the recorded SHA-256 inventory.

A second compatibility boundary check decodes all three frozen 0.1.11 transport
fixtures with both package versions and preserves their exact JSON values.
The frozen decoder explicitly rejects the new registration envelope. Existing
endpoint source and the local 0.1.11 contract are unchanged.

## Verification

AQ's installed `aq test` accepts pytest arguments. The retained
[B1 server adapter](B1-server-evidence/test_cargo.py) runs exact Cargo commands
inside that resource gate with `CARGO_BUILD_JOBS=2`, dev debug info disabled and
incremental compilation disabled. The adapter caps Rust test threads at two
for database resource control. `B1_SERVER_CHECK` selects a focused check.
Cargo logs are retained next to the adapter as `.txt` files.

- `B1_SERVER_CHECK=format aq test docs/implementation/B1-server-evidence/test_cargo.py -q --timeout=1800`:
  **pass**, executing `cargo fmt -p aq-server --check` ([log](B1-server-evidence/format.txt)).
- `cargo fmt --all --check`: **unavailable** in this nested worktree: Cargo
  resolves an existing vendored lifecycle crate to the outer checkout's workspace.
  The owner reported the same limitation. Full error retained in
  [format-all-unavailable.txt](B1-server-evidence/format-all-unavailable.txt).
- Independent tar-member comparison: **119/119 files identical** to the pinned archive.
- `git diff --check`: **pass**.

- `aq test docs/implementation/B1-server-evidence/test_cargo.py -q --timeout=1800`:
  **pass**, executing required `cargo test --locked --workspace permit_fixture_conformance`.
  All **3** matching server tests passed; no test failures.
  [Cargo log](B1-server-evidence/conformance.txt).
- `B1_SERVER_CHECK=legacy aq test docs/implementation/B1-server-evidence/test_cargo.py -q --timeout=1800`:
  **pass**, executing `cargo test --locked -p aq-admission@0.1.11 --test transport --test preparation_v2 --test preparation_refresh --test granted_closure`.
  All **40** tests passed (4 transport, 16 preparation-v2, 15 refresh, 5 closure).
  [Cargo log](B1-server-evidence/legacy.txt).

The extra database-backed endpoint area command was
`cargo test --locked -p aq-server --test native_registration --test native_initial_grant`,
through the same AQ adapter with `DATABASE_URL` supplied by `scripts/test-db`:

- Default Rust concurrency: **29 passed, 12 failed, 13 existing ignored** in
  `native_initial_grant`; Cargo stopped before registration.
  [Complete log](B1-server-evidence/endpoints-unbounded.txt).
- `RUST_TEST_THREADS=2`: **38 passed, 3 failed, 13 existing ignored**.
  [Complete log](B1-server-evidence/endpoints-threads2.txt).
- `RUST_TEST_THREADS=1`: **40 passed, 1 failed, 13 existing ignored**.
  The sole failure was
  `published::exact_full_artifact_and_material_receipt_coverage_is_mandatory`,
  at `published.rs:289`, returning `selection expired during admission locks`.
  [Complete log](B1-server-evidence/endpoints-serial.txt).
- The sole serial failure then passed **1/1**, unchanged and run in isolation:
  `DATABASE_URL=<scripts/test-db URL> RUST_TEST_THREADS=1 B1_SERVER_CHECK=endpoint_retry aq test docs/implementation/B1-server-evidence/test_cargo.py -q --timeout=1800`,
  executing `cargo test --locked -p aq-server --test native_initial_grant published::exact_full_artifact_and_material_receipt_coverage_is_mandatory -- --exact`.
  [Complete log](B1-server-evidence/endpoint_retry.txt).

These results indicate sensitivity to concurrent load and short fixture deadlines;
they do **not** establish a wholly green endpoint area run. Every endpoint case
that failed under greater concurrency passed either in the serial area run or
the focused retry. No assertion, production endpoint, frozen contract source,
fixture deadline or ignore annotation was changed. The 13 existing ignores require
explicit actual-worker/Git/tmux artifacts and were not enabled or counted as passes.
Supervisor reports: `msg-8e821ecd0c1542e0ad6068aae1b550bd` and
`msg-16467ae888fe4eb88719d03b5dfc376e`.

- Registration-only check: **4/4 passed**, using
  `DATABASE_URL=<scripts/test-db URL> RUST_TEST_THREADS=1 B1_SERVER_CHECK=registration aq test docs/implementation/B1-server-evidence/test_cargo.py -q --timeout=1800`,
  executing `cargo test --locked -p aq-server --test native_registration`.
  [Complete log](B1-server-evidence/registration.txt).

## Final sync

The startup instructions name `origin/dev`, which does not exist here.
Supervisor message `msg-e7d1e5163564498d859cdde2b0cf5169` explicitly authorizes
merging fetched `origin/main` for final sync, without creating dev or pushing main.
The supervisor also reports owner PR #12 merged at `ff97d55`; the recorded
`c413cfe2` source revision and archive checksum remain the exact pin.

The fetched main revision was `18af3900f5fae76bdc9797017cd4cbd9f4987b8a`.
Merge commit `2b0f1a7315e5b4afc9905de3eb474fc5980f9629` merged it cleanly after
the single implementation commit `45154f35`; incoming changes were confined
to three retrospective/build-supervision documents.
Post-merge `B1_SERVER_CHECK=postmerge aq test docs/implementation/B1-server-evidence/test_cargo.py -q --timeout=1800`
passed, executing `cargo test --locked -p aq-server --test permit_fixture_conformance`:
**3/3 passed** ([log](B1-server-evidence/postmerge.txt)).

Supervisor disposition `msg-4cf73eff1401404f91b9ec8a9808ef36` accepts the
shared-PostgreSQL legacy timing failures as pre-existing and outside this task,
directs honest evidence with no area-green claim or legacy source changes, and
authorizes publication and close after registration verification. The supervisor
will track the flakiness separately. Registration passed 4/4 as recorded above.
