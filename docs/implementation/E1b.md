# E1b: pinned dashboard server image

Task `smart-falcon-16.3`; source base `2917c84e`. The final pushed SHA is recorded in the task close/comment because this document belongs to that commit.

## Release input and boundary

The server consumes E3's published dashboard **v0.1.0**, source `4bfc637176d28d5748d2ba0a733a21ef95081a90`, without rebuilding dashboard sources or depending on a sibling checkout. The vendored archive and original `.sha256`/`.release.json` are the downloaded GitHub release assets from E3's retained download cache. Their metadata is pinned in `vendor/dashboard-candidate.json`.

- Release archive URL: `https://github.com/ElectricJack/aq-dashboard/releases/download/v0.1.0/aq-dashboard-v0.1.0.tar.gz`.
- Archive SHA256: `162f2cd29e7123570f4568df68ff2ffffa1c97417c1f51369fcd55b1c7bd2465`.
- Exact manifest digest/bootstrap identity: `sha256:f1eefcaafe25426f84b53ea306efed0110e8629f49a09e7e67fcff5b7f3580da`.
- 29 listed files; manifest schema 2.

The Docker server stage verifies the archive before extracting to `/usr/share/agent-q/dashboard` and supplies `AQ_DASHBOARD_DIR`. Production Compose already has no dashboard bind mount. `compose.dashboard.yaml` is explicitly a development-only override.

## Compatibility and caching

Schema 1 remains supported. Schema 2 has typed, required release provenance/toolchain and explicit API/capability requirements. Capability requirements identify implemented protocol vocabulary, independently of enabled grants and optional mounted owners for each authenticated principal. Unsupported requirements fail startup. Existing single-link/regular-file protections and limits remain: 512 files, 32 MiB assets, 256 KiB manifest.

Every asset, index and manifest request still extracts `CurrentUser`; authenticated hashed assets get `private, max-age=31536000, immutable`, while index, SPA routes, compatibility metadata and unhashed assets use `no-cache`. API responses and errors use `no-store`; existing explicitly private no-store responses retain that policy.

## Verification environment and policy

Linux x86_64 isolated AQ worktree; pinned Rust toolchain and Cargo.lock; `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`; PostgreSQL on loopback port 55432 from `scripts/test-db`. AQ has a pytest runner, so `.aq/tmp/e1b/test_cargo.py` runs each command as one subprocess under `aq test` resource admission. The adapter is retained as `docs/implementation/evidence/E1b/test_cargo.py`. Exact child commands and results are recorded below; full logs are retained in `docs/implementation/evidence/E1b/`.

Worker policy calls for focused and affected-area checks; full workspace execution is CI-owned. The required named Cargo filter is still run exactly as specified. No green full-suite claim is made. E1b has no adversarial-review marker.

## Qualification limits

E3's published record discloses existing frontend full-suite failures tracked in aq-dashboard task `keen-vault-60`; this server packaging task does not qualify or change the dashboard UI. No dashboard asset was rebuilt or altered.

The Cargo acceptance test checks the same vendored archive and Docker checksum/destination/environment wiring, loads its exact bytes with the server loader, and exercises authenticated HTTP router/bootstrap responses using PostgreSQL. It does not itself build or launch the complete production Docker image. A separate Docker smoke built the exact production dashboard COPY/RUN/ENV fragment on Debian, verified the archive with `sha256sum -c`, and checked the installed manifest digest as UID 10001 in a read-only, network-disabled container. It confirmed `AQ_DASHBOARD_DIR` and the expected manifest SHA; the temporary image was removed ([Docker smoke log](evidence/E1b/docker-bundle-smoke.log)). The complete production server image was not built or launched.

## Test-first evidence

- `cargo test --locked -p aq-server --test dashboard_assets server_image_serves_pinned_dashboard_digest`: expected failure, exactly **1 test failed**, because schema 2's `version` is an unknown manifest field ([red image log](evidence/E1b/red-image.log)).
- `cargo test --locked -p aq-server --lib schema_two_checks_required_contracts_capabilities_and_provenance`: expected failure, exactly **1 test failed**, because the schema 2 fixture cannot load ([red loader log](evidence/E1b/red-loader.log)).
- `python3 scripts/test-dashboard-candidate.py`: **4 passed**, covering the published archive, exact manifest extraction, corrupt archive, release-record drift, path traversal, symlink and duplicate members, manifest digest drift, and Docker checksum drift ([checker tests](evidence/E1b/pin-tests.log)).
- `python3 scripts/check-dashboard-candidate.py`: pass; both pinned digests and 29 file hashes/sizes agree ([pin verification](evidence/E1b/pin-check.json)).

The resource-admitted command shape is:

```sh
E1B_COMMAND='cargo test --locked -p aq-server --test dashboard_assets' \
E1B_LOG=docs/implementation/evidence/E1b/green-assets.log \
aq test docs/implementation/evidence/E1b/test_cargo.py -q
```

Shared-disk pressure required removal of 14 GiB of stale local incremental caches and 2.9 GiB of superseded executable variants, all older than this task's first build. All source/evidence were retained, and a resource heads-up was sent to the project supervisor. The executable cleanup was too broad: test target names overlap across packages, and Cargo had already marked one older `aqv2` CLI executable fresh. The first exact workspace run executed the named digest test successfully but then exited 101 because that CLI executable was absent. The failed invocation is retained in [cache recovery evidence](evidence/E1b/named-workspace-cache-failure.log). Rerunning the exact command lets Cargo rebuild the missing output; no product code was changed for this recovery. No further executable cleanup was performed.

## Completed implementation and retained checks

Implementation commit `a32bdadefa38112a847c6743eb2e16649283ebc9` squashes the test-first plan and code history onto the recorded source base. Merge `15c27f8185bd84898198aeff54c27490f146ca15` incorporates `origin/main` at `ff56fbba36a0bb0e8a42769cf8e2db66a1cfecf9` without conflict. That upstream change affects the dashboard aggregation credential fixture and documentation. The exact workspace acceptance command passed on that merge, with exactly one matching test. Final merge `840230374596d01eb2435427694b22b795eca812` then incorporates `origin/main` at `ed027c42112bed58569ed035803389d98012686f`, including independently delivered central-delivery persistence and state format 45. No conflict occurred; the pin checker passed again. Final all-target Clippy, loader and affected dashboard suites passed on this combined tree; the dashboard area suite includes the same named acceptance test.

| Command | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked -p aq-server --test dashboard_assets` | 4 passed, including the pinned digest acceptance test | [focused router log](evidence/E1b/green-assets.log) |
| `cargo test --locked --workspace server_image_serves_pinned_dashboard_digest` | passed; exactly 1 matching test | [workspace acceptance log](evidence/E1b/named-workspace.log) |
| `cargo clippy --locked --all-targets -- -D warnings` | passed before and after final merge | [initial log](evidence/E1b/clippy.log), [combined-tree log](evidence/E1b/clippy-after-merge.log) |
| `cargo fmt --check` | passed on final merged tree | [final format log](evidence/E1b/fmt-final.log) |
| `cargo test --locked -p aq-server --lib dashboard_assets` | 6 passed on final merged tree; quotas, path/type/link protection and compatibility/cache assertions | [loader log](evidence/E1b/loader-tests.log) |
| `cargo test --locked -p aq-server --test dashboard_assets --test dashboard_read --test dashboard_actions --test dashboard_mounts --test dashboard_aggregation` | 27 passed: actions 8, aggregation 5, assets 4, mounts 3, read 7; includes 1 named digest test on final merged tree | [dashboard area log](evidence/E1b/dashboard-area.log) |
| `python3 docs/implementation/evidence/E1b/docker_bundle_smoke.py` | passed; actual bundle installation fragment and nonroot read verified | [Docker smoke log](evidence/E1b/docker-bundle-smoke.log) |

The source changes are confined to the release inputs/checker, Docker packaging and development overlay, CI pin verification, typed dashboard manifest compatibility, authenticated asset caching, and their focused tests. Canonical architecture and specification documents were not edited by this task.

The final grouped verification was admitted as one `aq test` adapter case and passed, with every child command exiting zero ([command/result transcript](evidence/E1b/focused-checks.log)). `verify-focused.py` retains the exact sequential commands. Definition-of-done checks are complete; no acceptance item is waived. Publication SHA and explicit task close are recorded in the AQ task result.

Final submission merge `b31053c532a6dbd9ec494046b0dbe7b991524dbd` incorporates upstream `e74a45e0844b76e3ab402a60f220f3a90eca0beb`. Its difference from the verified source merge is documentation only; no implementation changed after the passing combined-tree checks. The final explicit `git merge origin/main` reported already up to date.
