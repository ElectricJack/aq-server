# Format9 native process integration

Server test source `2297616e524f1d5badba02b63b24f98f4037eab1` changes only three journal assertions8→9 in two files. Exact `git show` source hashes and retained log SHA256 values appear in qualification.json. Worker source `8c813d664e33477c53155d29fc328d43e3a3795f` incorporates host `40d6e36057456e9f58c63dd2b2509ccf412e38db`; the exact tested release binary SHA256 is `42f7a940f8c71b1b3771c50a5125f782158d1b1bd66a6ea8f7f050dd763b18d8`. Git pins are unchanged.

The requested server no-run compile passed33.86s and worker release build passed46.36s. Both exact independent process cases passed: phase execution5.78s (journal9/rev27) and existing phase-input-only7.15s (journal9/rev26). Execution issuance occurred1747ms after original proposal expiry via natural input work. The actual worker receives and durably stores live execution caps; input-only retains late replay/input assertions and no execution issuance. Policies remain phase2s/selection5s, with no deadline retiming. Explicit formatting passed.

These cases use actual worker processes, daemon HTTP/SQL and aq-git/Git. Graph, model/provider/runtime and resource qualification remain synthetic; no host Started event, paid provider, production route activation or full production acceptance is claimed. No full legacy suite or new broad lint was run for three numeric assertions. Root preview is unchanged. Earlier format8 evidence remains immutable and is not relabeled.

Commands:

Rerun commands below use the leased slot target and disable incremental builds;
see [Cargo worker build guidance](../../guides/cargo-worker-builds.md).
Historical commands, results and hashes remain in the machine-readable evidence.

```sh
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-server --test native_initial_grant --all-features --locked --offline --no-run
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=1 AQ_TEST_GIT_CANDIDATE=/tmp/aq-wrk08-git-pin/aq-git AQ_TEST_GIT_EXECUTABLE=/usr/bin/git AQ_TEST_NATIVE_WORKER=/tmp/aq-native-prepare-smoke-artifact/phase9-execution-release/native-prepare-smoke AQ_TEST_NATIVE_WORKER_SHA256=42f7a940f8c71b1b3771c50a5125f782158d1b1bd66a6ea8f7f050dd763b18d8 cargo test -p aq-server --test native_initial_grant --all-features --locked --offline published::actual_git::worker_assembly::actual_worker_receives_durable_phase_execution_from_actual_issuer -- --exact --ignored --test-threads=1 --nocapture
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=1 AQ_TEST_GIT_CANDIDATE=/tmp/aq-wrk08-git-pin/aq-git AQ_TEST_GIT_EXECUTABLE=/usr/bin/git AQ_TEST_NATIVE_WORKER=/tmp/aq-native-prepare-smoke-artifact/phase9-execution-release/native-prepare-smoke AQ_TEST_NATIVE_WORKER_SHA256=42f7a940f8c71b1b3771c50a5125f782158d1b1bd66a6ea8f7f050dd763b18d8 cargo test -p aq-server --test native_initial_grant --all-features --locked --offline published::actual_git::worker_assembly::actual_worker_prepares_phase_inputs_after_proposal_expiry -- --exact --ignored --test-threads=1 --nocapture
rustfmt --edition 2024 --check crates/aq-server/tests/native_initial_grant/published/actual_git/worker_assembly.rs crates/aq-server/tests/native_initial_grant/published/actual_git/worker_assembly/phase.rs
```
