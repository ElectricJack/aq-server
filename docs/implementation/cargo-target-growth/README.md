# Cargo target growth qualification

Task `fair-pinnacle-63`, 2026-11-02. The aq-server workspace now uses dev/test
line tables, no dependency debug information, and no incremental compilation.
Cargo-invoking Python scripts also force `CARGO_INCREMENTAL=0`, including when
qualifying extracted package sources. Rerun guidance reuses the leased slot target.

## Controlled clean build comparison

Both builds ran in slot-4 using Rust 1.96.1 and unchanged Rust sources/lockfile
from `e74a45e0844b76e3ab402a60f220f3a90eca0beb`. Only the root Cargo profiles
changed between builds. Each started with `cargo clean --target-dir "$PWD/target"`
and ran the same command, with no debug or incremental environment overrides:

```sh
/usr/bin/time -v env CARGO_BUILD_JOBS=2 cargo test -p aq-server --all-features --no-run
```

Both exited 0 and linked all 108 integration targets. Measurements were taken
immediately after each build, before the backtrace probe or focused tests.
No other slot's target was cleaned or modified.

| Measurement | Before | After |
| --- | ---: | ---: |
| Target allocated bytes (`du -s -B1`) | 66,990,444,544 (62.390 GiB) | 15,374,602,240 (14.319 GiB) |
| Target apparent bytes | 67,027,029,936 | 15,467,592,361 |
| Incremental allocated bytes | 11,080,667,136 (10.320 GiB) | 4,096 (empty directory) |
| Integration executable bytes | 49,404,647,352 | 13,511,965,544 |
| Representative `profiled_results` executable bytes | 543,793,528 | 175,406,240 |
| Representative `.debug_*` section bytes | 412,313,738 | 49,837,484 |
| Observed wall time | 33:08.56 | 14:44.63 |

Allocated target size decreased **77.05%**, saving 48.071 GiB. The incremental
directory had no children after the second build. These are clean-build artifact
measurements; repeated feature combinations can still accumulate artifacts.
Wall times reflect a shared host and are not a performance guarantee.

The retained [context](context.json), [measurement script](measurement-script.txt),
[before metrics](before-metrics.json), [after metrics](after-metrics.json),
[before build](before-build.txt), [after build](after-build.txt),
[before clean](before-clean.txt), and [after clean](after-clean.txt) make the
comparison auditable. Section sizes come from [before readelf](before-readelf.txt)
and [after readelf](after-readelf.txt). Observed workspace compiler arguments
confirm `debuginfo=line-tables-only` and no incremental argument in
[after compiler flags](after-compiler-flags.json).

## Focused verification

- `aq test scripts/test-cargo-qualification.py`: **5 passed**. Exercises child
  environments with inherited incremental compilation enabled, explicit target
  reuse, and the default profile-package target. Child Cargo commands are stubbed;
  this verifies the environment contract without recompiling package archives.
  [Output](aq-test.txt).
- `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo test -p aq-server --all-features --lib profiled_execution::settlement::eligibility::tests -- --test-threads=2`:
  **6 passed**, 185 filtered out. [Output](focused-rust-tests.txt).
- A temporary workspace example ran with
  `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo run --quiet -p aq-server --all-features --locked --offline --example cargo_profile_backtrace_probe`.
  `Backtrace::force_capture()` retained both workspace frames with exact
  `cargo_profile_backtrace_probe.rs:3:20` and `:7:5` locations.
  [Source](backtrace-probe-source.txt), [output](backtrace-probe-output.txt).
  The temporary example was removed after verification.
- Changed Python files and injected fixture blocks parse; guidance links resolve;
  `git diff --check` passes. No database or full runtime suite was required.

The supervisor split other Rust repositories into sibling tasks. Changes to the
`aq test` runner, slot target caps, and disk-pressure admission belong to
agent-queue and are outside this aq-server change. See
[worker build guidance](../../guides/cargo-worker-builds.md) for future runs.
