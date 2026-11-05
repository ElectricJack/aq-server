# Cargo builds in AQ worker slots

Run Cargo from the root of your assigned slot. Reuse that slot's own target
directory for focused tests, lint and extracted-package qualification:

```sh
export CARGO_TARGET_DIR="$PWD/target"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2
cargo test -p aq-server --locked --offline --test dashboard_actions
```

Capture the absolute target path before changing into an extracted source tree;
pass the same environment to child Cargo processes. Temporary source extraction
is fine, but do not allocate another target under `/tmp`, share another slot's
target, or use an owner's base-checkout target. Only clean your own leased target.

The workspace dev profile uses `debug = "line-tables-only"` for workspace crates,
`debug = false` for external dependencies and `incremental = false`. Cargo's test
profile inherits these settings. Workspace backtraces retain file:line locations.
Keep `CARGO_INCREMENTAL=0` in test-runner environments as well, including for
extracted packages whose manifests do not inherit this workspace's profiles.
Do not set `CARGO_PROFILE_DEV_DEBUG=0` or `CARGO_PROFILE_TEST_DEBUG=0` when checking
backtraces.

`aq test --aq-help` describes the installed runner. It currently accepts pytest
and Bazel, so use foreground Cargo with the limits above for Rust checks. Use
`aq test` for the applicable Python checks without increasing its worker cap.

AQ-08 qualification JSON, source hashes and raw logs retain the historical
commands and observations. For reruns, apply this target/environment guidance to
the listed focused test selection and preserve each fixture's external artifact,
database and digest requirements. A historical `/tmp` target path is not a new
target allocation instruction. Rerun commands in qualification Markdown use the
slot target; their historical results remain attributed to the recorded sources.
