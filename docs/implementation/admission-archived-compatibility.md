# Admission archived source compatibility repair

Task: `eager-grove-45.12`. Source base: `cc7b74e`.

The 0.1.7 control contract promises exact preservation of the archived 0.1.6
source and publication bytes, with an append-only exception for `src/lib.rs`.
See [the control qualification](aq-admission-0.1.7-qualification.md) and
`crates/aq-admission/README.md`. The expected hashes are correct.

Commit `7a10417` added interactive clock methods and extracted shared helpers
inside the frozen `src/transport.rs`. Its resulting SHA256 was
`223682461648bbb6eae9bcf5ca17cc045982a3eca3ee46e6a09f6c35c418ea52`, breaking
the archived-byte assertion reported by A4 (commit `a427134`,
`docs/implementation/A4.md`).

The repair restores the exact 35,252-byte `src/transport.rs` from the retained
0.1.6 archive. That archive's SHA256 is
`d74db09f5766a437b9d43b6d63710a56afc4db69432953ef06699ec7ab09e066`, and its
transport source SHA256 is
`4f19c6c0bb728e781fb6349c4a994e28c31b0ff665fe11628c70169df9ceea50`.
[The archive comparison](evidence/admission-compat-eager-grove-45.12/archive-check.json)
also verifies all 32 control, 63 granted-closure and 71 preparation-v2 inventory
rows, using each inventory's recorded prefix exception for `lib.rs`.

The three interactive methods remain inherent public methods on `DeliveryClock`,
implemented in the new private `interactive_delivery` module. Existing callers
keep their method paths, one-hour bound, validation errors, and checked deadline
arithmetic. The normal clock keeps its original two-minute bound. The only
`lib.rs` change appends a module declaration. Frozen inventories, schemas,
fixtures and compatibility assertions are unchanged.

## Validation

This installation's `aq test` supports pytest/Bazel rather than Cargo, as already
recorded in A4. Cargo validation uses `CARGO_BUILD_JOBS=2`,
`RUST_TEST_THREADS=2`, `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, and
`CARGO_PROFILE_TEST_DEBUG=0`, with a slot-local target directory.

- `cargo test --locked -p aq-admission --test control
  frozen_six_source_and_publication_bytes_match_the_exact_archived_baseline
  -- --exact`: passes, one test, nine filtered.
  [Log](evidence/admission-compat-eager-grove-45.12/frozen-six.log).
- `cargo test --locked -p aq-admission`: passes, 98 tests, zero failures,
  ignored tests or filtered tests. This includes all three archive-preservation
  tests and `long_delivery_is_explicit_and_keeps_original_clock_bounds_and_contract`.
  [Log](evidence/admission-compat-eager-grove-45.12/admission.log).
- `rustfmt --edition 2024 --check --config skip_children=true
  crates/aq-admission/src/lib.rs crates/aq-admission/src/transport.rs
  crates/aq-admission/src/interactive_delivery.rs`: passes.
- `git diff --check`: passes.

The requested `cargo test --locked --workspace` ran with `DATABASE_URL` set to
`postgres://postgres:test@127.0.0.1:55432/postgres`, the existing disposable service
documented by `scripts/test-db`. It exited 101 after 499 passed, 11 failed and
two ignored tests across completed targets. Admission preservation tests pass.
[Full workspace log](evidence/admission-compat-eager-grove-45.12/workspace.log).

The run stops in `aq-server --lib` (151 passed, 11 failed, one ignored):

- Nine `agent_access::messaging::global::reservations::tests` cases fail at
  `tests.rs:21`: the fixture asserts state format 37 but receives 41.
- `store::central37_tests::retained_owner_reserve_and_cancel_proofs_are_revalidated_after_guards_reenabled`
  fails at `central37_tests.rs:239` with `IncompatibleState { found: 41,
  supported: 37 }`.
- `store::settlement39_tests::settlement39_registered_upgrade_old_reader_and_critical_drift`
  fails at `settlement39_tests.rs:36`: `migrate_known(&pool, MIGRATIONS, 39)`
  supplies the full migration list, producing a state-format metadata mismatch.

These server sources and fixtures are unchanged by this repair. Follow-up
`eager-grove-45.13` owns their historical migration setup; this change preserves
all migration assertions. Later workspace targets are not claimed to have run.
The build also emits the example-name collisions already recorded by A4.

## Final synchronization

The repository's default remote branch is `origin/main`; `origin/dev` is absent.
The final task branch merges `origin/main` at
`e6babbafe807c311e508e1a7743cc6621da83117` in commit `076025e`.
`aq git pull --branch main` completed without conflicts, and the subsequent
`git merge origin/main` reported already up to date.

After synchronization, `cargo test --locked -p aq-admission` passes all 98 tests
again, with zero failures, ignored tests or filtered tests.
[Post-merge log](evidence/admission-compat-eager-grove-45.12/admission-after-sync.log).
The full workspace result above records the original repair validation; it was
not repeated after merging the default branch. Default-branch delivery remains
the integration owner's responsibility.
