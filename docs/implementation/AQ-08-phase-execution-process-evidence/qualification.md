# Actual worker phase execution process qualification

Probe source `013ffe881b73c7d2d8f2bf25444040355a59ca2c` changes exactly two test files on root base `ea1ca6e23d79ae931ec1d9a40ab2e752cb0b23d6`. The process ran against frozen issuer `3af7741c3a9c7b3e006718dad7980f3904605119`; the listed production paths are byte-equivalent between that issuer and the probe base. Source inventory hashes use exact `git show` bytes. Worker source is `376c2b3af1345bf5717ab7212bbf23382548c105`; exact worker/Git artifact hashes and all retained log hashes are in qualification.json.

The actual worker→daemon HTTP/SQL→actual aq-git/Git process passed1 case in6.23s. It performed actual native registration before readiness, real V2 inventory ownership and a new authenticated operator observation before Poll, acceptance,38 artifact receipts, private material installation, Git hydration, fresh actual issuer confirmation and durable received execution rev27. Issuance was2079ms after the original proposal expired through natural input work, with unchanged phase2s/selection5s observation policies. The new execution mode alone omits an artificial expiry-test pause; earlier three modes keep their assertions and behavior.

The test verifies exact execution receipt/nonce/request and retained SQL material receipt, canonical durable record digest, original accepted history, local live execution/hard caps, held claim and absence of Started/legacy prepare receipts. It wraps the actual issuer in an explicit test-only route; it adds no production route. Graph, model/provider/runtime and resource qualifications remain synthetic. No physical harness/provider call or settlement is claimed.

Final strict all-target/all-feature server Clippy passed2.36s; its owner independently matched both candidate files to this exact probe source. Explicit rustfmt checks passed. The full native aggregate remains39pass/2fail/7ignore, not a broad green result. One unchanged isolated rerun of each failing case passed1.32s/1.61s without deadline or assertion changes.

Development negatives are retained: one test module-path compile error; registration attempted before the child registered; stale provider source age6183ms correctly refused; and an incorrect private-journal audit field after actual grant success. Corrected handshake/pause behavior applies only to the new worker execution probe. These negatives are not counted as passing qualification.

```sh
CARGO_TARGET_DIR=/tmp/aq-phase30-qualification-target CARGO_BUILD_JOBS=2 DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres RUST_TEST_THREADS=1 AQ_TEST_GIT_CANDIDATE=/tmp/aq-wrk08-git-pin/aq-git AQ_TEST_GIT_EXECUTABLE=/usr/bin/git AQ_TEST_NATIVE_WORKER=/tmp/aq-native-prepare-smoke-artifact/phase8-execution-prompt-release/native-prepare-smoke AQ_TEST_NATIVE_WORKER_SHA256=077e21136716a6826bb7df89cc8e8fb11d7cd9b3bb8e891c49626dcaddac6d79 cargo test -p aq-server --test native_initial_grant --all-features --locked --offline published::actual_git::worker_assembly::actual_worker_receives_durable_phase_execution_from_actual_issuer -- --exact --ignored --test-threads=1 --nocapture
```

External binaries are referenced by exact digest, not copied into this evidence directory. Source and compiler/build ownership remain separate from this process claim. No full production/fleet acceptance is implied.
