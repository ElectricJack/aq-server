# aq-admission 0.3.3 server pin

The sandbox dependency now consumes the immutable aq-worker release at
`3e54e08a11b6b560c789220b5ed13032a0fc87de`, from
[aq-worker PR #32](https://github.com/ElectricJack/aq-worker/pull/32).
This server change is stacked on that upstream PR. Supervisor guidance on
`solid-impact-24` authorizes the pushed source before upstream merge.
The pin source must be re-verified against main's release assets once #32
merges; upstream main `64db659115755ec083989738c13626714a07c497` did not
contain those assets when first checked.

The [pin inventory](../../vendor/admission-contract.json) records the immutable
archive URL, revision and every extracted member hash. The archive SHA-256 is
`35fb4440141a9985db63a17f3d632dc24d075bb97372d08cdbe192c5879bc2f9`.
All 120 files match both the upstream release manifest and the archive.
The Cargo path, exact version, workspace exclusion and lockfile use 0.3.3.

The consumer conformance test runs all 134 release fixtures, including
`shared_build_cache_supported` and `shared_build_cache_disabled`, and checks
the generated schema and source inventory. All previous 132 fixture documents
are preserved. A consumer test derives the supported set from a validated
node registration: requiring `shared-build-cache` refuses an existing node,
then accepts after explicit advertisement and wire round-trip.

SBX26 enablement remains opt-in per node through aq-worker's
`sandbox.shared_sccache` configuration. As the packaged README documents,
operators must never enable this writable cross-attempt cache on nodes running
untrusted projects. This pin update enables no node configuration or cache
mount. Frozen 0.1.11 consumers retain their installed dependency.

Focused validation uses:

```sh
aq test docs/implementation/evidence/admission033/test_cargo.py -q --timeout=1800
```

The [adapter](evidence/admission033/test_cargo.py) records exact Cargo commands
in [checks.txt](evidence/admission033/checks.txt), using this slot's target,
two build jobs, two Rust test threads and no incremental compilation.

Results: the AQ adapter passed (1/1, 362.43 seconds). Formatting passed;
`cargo test --locked --offline -p aq-server --test permit_fixture_conformance`
passed 5/5 tests; `cargo test --locked --offline -p aq-admission@0.3.3 --test sandbox`
passed 16/16 tests; and `cargo test --locked --offline -p aq-admission@0.1.11 --test transport`
passed 4/4 tests. No ignored tests or failures occurred in these selections.
`git diff --check` passed. The task branch was synchronized with `origin/main`
before validation; the final submission also merges `origin/main` before push.
