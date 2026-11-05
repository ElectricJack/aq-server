# E1b Dashboard Image Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Embed the verified E3 v0.1.0 archive in the server image and serve authenticated assets with correct caching and bootstrap digest.

**Architecture:** Vendor the published archive, checksum sidecar and release record without rebuilding dashboard sources. A dependency-free Python checker binds the release record, archive, exact manifest bytes and Docker instructions. Extend the startup loader with typed schema 2 provenance and compatibility requirements, retaining schema 1. Cache only verified hashed assets immutably; preserve no-store for APIs and errors.

**Tech Stack:** Rust/Axum/Serde/SHA256, Python standard library, Docker Compose, PostgreSQL test fixtures.

**Spec:** Task smart-falcon-16.3 / E1b in `2026-11-02-v2-full-application-tasks.yaml`; Global Constraints in `2026-11-02-v2-full-application-plan.md`.

## Global Constraints

- Builds use committed Cargo.lock with `--locked`; no sibling dependencies or floating release inputs.
- Preserve 512 files, 32 MiB bundle and 256 KiB manifest limits, single-link regular files and asset type/path restrictions.
- Unsupported required capabilities refuse explicitly; runtime authorization remains bootstrap-owned.
- Do not edit canonical architecture/spec documents or produce dashboard release assets.
- Record commands, environment, commit and limits in `docs/implementation/E1b.md`; publish the assigned branch through AQ.
- Run fmt, all-target clippy and focused/area Cargo tests under AQ resource controls. Full-suite execution belongs to CI per worker policy.

### Task 1: Release pin and acceptance proof

**Files:** Create `vendor/dashboard-candidate.json`, `vendor/aq-dashboard-v0.1.0.tar.gz`, its `.sha256` and `.release.json`; `scripts/check-dashboard-candidate.py`. Modify `Dockerfile`, `compose.dashboard.yaml`, `.github/workflows/ci.yml`, `crates/aq-server/tests/dashboard_assets.rs`.

**Interfaces:** Consumes E3 release v0.1.0 archive SHA256 `162f2cd29e7123570f4568df68ff2ffffa1c97417c1f51369fcd55b1c7bd2465` and exact manifest digest `f1eefcaafe25426f84b53ea306efed0110e8629f49a09e7e67fcff5b7f3580da`. Produces `/usr/share/agent-q/dashboard`, `AQ_DASHBOARD_DIR` and test `server_image_serves_pinned_dashboard_digest`.

- [x] Add a real Cargo test that invokes the checker to verify/extract the pinned archive into a temporary directory, loads `DashboardAssets`, requests authenticated `/api/v1/bootstrap`, and asserts `body["data"]["product"]["dashboard_digest"] == format!("sha256:{}", assets.digest())` and the pin's `dashboard_digest`; request `/` and a manifest-listed JS asset too.
- [x] Run `cargo test --locked -p aq-server --test dashboard_assets server_image_serves_pinned_dashboard_digest` through an `aq test` subprocess adapter. Expect failure because checker/pin/schema support do not exist yet.
- [x] Copy E3's published inputs from the downloaded release cache only after checking SHA256. Pin release tag, URL, archive and manifest digests and source revision. Checker must reject digest/record/manifest mismatches and unsafe archive members before extraction, verify listed bytes/hashes, and check the Docker checksum/path/environment wiring.
- [x] In the server stage, COPY the vendored archive, RUN `echo '<archive digest>  /tmp/dashboard.tar.gz' | sha256sum -c -` before `tar -xzf ... -C /usr/share/agent-q/dashboard`, remove the temporary archive, and ENV `AQ_DASHBOARD_DIR=/usr/share/agent-q/dashboard`. Production Compose needs no mount; label overlay development-only. Add checker to CI.

### Task 2: Typed compatibility and HTTP caching

**Files:** Modify `crates/aq-server/src/dashboard_assets.rs`, `crates/aq-server/src/lib.rs`, and `crates/aq-server/tests/dashboard_assets.rs`.

**Interfaces:** `DashboardAssets::load(directory: &Path) -> anyhow::Result<Self>` retains digest semantics. Schema 2 includes version, source {revision,dirty,official}, toolchain {node,pnpm,lockfile_sha256}, required_api_versions and required_capabilities. Bootstrap advertises dashboard-read-1 and dashboard-actions-1; capability identifiers are those implemented in `dashboard_api/service.rs`.

- [x] Add loader tests using a schema 2 fixture: accept supported requirements and development provenance; refuse unknown API/capability, missing required schema 2 metadata, malformed revision/lock digest, unknown fields and future schema. Existing security/quota tests stay intact.
- [x] Add router assertions: GET/HEAD `/index.html`, `/` and SPA routes use `no-cache`; manifest metadata uses `no-cache`; hashed asset uses `private, max-age=31536000, immutable`; unhashed `assets/app.js` uses `no-cache`; authenticated and unauthenticated `/api/*` and errors remain `no-store`. Update former 404 expectations for newly served index/manifest.
- [x] Run focused loader/router tests and retain red evidence.
- [x] Implement typed optional schema 2 fields with deny_unknown_fields, require them for schema 2, and validate each required API/capability against the server's implemented vocabulary. Per-principal enabled grants remain independent. Keep all existing bounds and startup integrity checks.
- [x] Store the exact manifest bytes as an authenticated compatibility metadata response. Add cache policy to verified assets; only names with a final Vite-style `-<8+ alphanumeric/_ token>` before extension are immutable. Index/metadata/unhashed assets revalidate.
- [x] Update outer hardening to preserve explicit successful asset cache headers, force APIs/errors to no-store and retain existing private no-store semantics. Run acceptance and dashboard area tests green.

### Task 3: Verification and publication

**Files:** Create `docs/implementation/E1b.md` and retained logs in `docs/implementation/evidence/E1b/`.

- [x] Run `python3 scripts/check-dashboard-candidate.py`, corruption/unsafe archive checks, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, named command `cargo test --locked --workspace server_image_serves_pinned_dashboard_digest`, and dashboard assets/read/actions area tests under `aq test` with `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, PostgreSQL from scripts/test-db. Keep full logs and distinguish unavailable checks.
- [x] Reconcile every contract item, document no Docker build claim unless executed and E3's disclosed frontend qualification limits.
- [x] Settle task checklist including review not required (no adversarial marker), squash own unpublished implementation history onto recorded source base, merge origin/main, rerun targeted checks if merge changes relevant files, push via `aq git push`, record exact SHA, close pass with changes/verification/test and `--work-outcome shipped`, then `aq session drain-ack`.
