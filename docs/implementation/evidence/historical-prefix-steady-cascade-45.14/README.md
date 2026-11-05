# Historical migration prefix verification

Task: `steady-cascade-45.14`. Source checked: `d6ed24319745d8030e71c526b498b83f5bdde5e0`.

Both requested repairs were already present in source ancestor
`3d9ddc43d580ba93a9f8d520abadc80781600bf0`:

- `crates/aq-server/src/store/central37_tests.rs:198-264` opens
  `db.store_at_version(37)` and validates reserve/cancel proofs before and after
  corruption, retaining the no-mutation assertions.
- `crates/aq-server/src/test_support.rs:82-94` and
  `crates/aq-server/src/store.rs:627-633` build the historical Store through the
  existing registered migration-prefix runner and normal pool initialization.
- `crates/aq-server/src/store/settlement39_tests.rs:27-69` upgrades with
  `&MIGRATIONS[..39]`, retaining the preserved-conversation, old-reader refusal
  and seven catalog-drift checks.

No additional fixture or production changes are necessary. Current state format
is 43. The current registry test checks contiguous migrations, the latest
state-format constant, readable formats and each migration's metadata update.
The integration Store tests also cover current migration idempotence, future
migration/state refusal, and real state2-to-current upgrade preservation.

The installed `aq test` supports pytest/Bazel. The retained `admitted_checks.py`
adapter runs the following Rust checks sequentially under one foreground AQ
admission, preserving its worker defaults and marker selection:

```sh
scripts/test-db
aq test docs/implementation/evidence/historical-prefix-steady-cascade-45.14/admitted_checks.py -q
```

Each `TestDb` creates and drops its own disposable PostgreSQL database. Cargo
uses one build job, disabled incremental/debug information, and two Rust test
threads, with the existing slot-local target cache. The exact Cargo commands
and full output are retained in `state-format.log`, `store-unit.log`, and
`store-integration.log`. The adapter stops on any unsuccessful command.

Initial verification used the equivalent adapter at
`.aq/tmp/test_historical_prefix.py` via
`aq test .aq/tmp/test_historical_prefix.py -q`: **1 adapter passed**, covering
**1 state-format check, 25 Store unit tests and 14 Store integration tests**, with
zero failures or ignored tests. The state-format check is also included among
the 25 Store unit tests. `initial-admission.log` records the slot, unchanged AQ
worker/marker defaults and pytest result.

Supervisor message `msg-6cf4320edfe547ab85799079f87dd741` explicitly confirmed
`origin/main` as the final merge source because `origin/dev` does not exist and
the train is off. It authorized verification-only completion of the repairs
already in ancestor `3d9ddc43`; no new code is needed. The guarded
`aq git pull --branch main --brief` fetched and merged main successfully; main
and HEAD remained `d6ed24319745d8030e71c526b498b83f5bdde5e0` (already up to date).

Final verification after that merge step used the retained adapter command above:
**1 adapter passed**, again covering **1 state-format check, 25 Store unit tests
and 14 Store integration tests**, with zero failures or ignored tests. Its AQ
admission and pytest result are in `final-admission.log`; the three Cargo logs
contain the final results. `git diff --check` passed. No full-suite run or
default-branch delivery is claimed.
