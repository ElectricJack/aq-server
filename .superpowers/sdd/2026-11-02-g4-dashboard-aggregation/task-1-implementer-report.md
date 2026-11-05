# Quota slice implementer report

Status: DONE (quota slice only; the overall G4 task remains incomplete).

Implemented an owner-authorized provider-usage route, account-deduplicated inventory projection, explicit validated Codex source configuration, fixed-authority bounded collector, single-flight/cache and stale/unknown behavior. Added quota DTOs, schema and synthetic fixtures, bootstrap capability gating, HTTP and synthetic protocol tests, and source/security qualification in `docs/implementation/G4-quota.md`. No live credentials or provider requests were used. Source configuration maps a symbolic inventory account explicitly to its ChatGPT account ID. Additional groups are bounded to eight; responses exceeding this bound are rejected as unknown/stale rather than silently truncated.

Test-first evidence and final checks, all run through AQ test admission with CARGO_BUILD_JOBS=2, RUST_TEST_THREADS=2 and the disposable test database:

- RED: `cargo test --locked --workspace --test dashboard_aggregation quota_unknown_is_not_zero` ran one test and failed on the then-unmounted route (404); `docs/implementation/evidence/G4/quota-red-cargo.log`.
- GREEN: the same named command ran one test and passed; `quota-final-cargo.log`.
- `cargo test --locked -p aq-server --lib quota_` ran two tests and passed; `quota-unit-fixed-cargo.log`. The synthetic loopback protocol test covers headers, concurrent collection, missing windows, additional groups, stale retention, errors, mismatch, redirect refusal, size and credential protections.
- `cargo test --locked -p aq-server --test dashboard_mounts` ran three tests and passed; `quota-mounts-cargo.log`.
- `cargo run --locked -p aq-server --example dashboard_contract` regenerated the schema; `quota-schema-generate.log`. The admitted Python `protocol/dashboard-read/v1/check.py` passed existing and new examples and negative cases; `quota-schema-check.log`.
- `cargo fmt --check` and `cargo clippy --locked -p aq-server --lib -- -D warnings` passed; `quota-fmt.log` and `quota-clippy.log`.

The initial broad workspace-filter build was stopped because it linked unrelated integration binaries; the controller approved the owning test-binary selection above. Broader workspace suites are CI-owned. Synthetic fixtures establish protocol behavior, not an authenticated live-account drill. Current task join failures produce a partial response; unexpected task panics may omit an account rather than return an unknown entry. The integrated review should assess that edge case. Overview/search are outside this slice; incomplete drafts were moved to ignored `.aq/tmp/g4/overview-draft.patch` and `.aq/tmp/g4/search-draft.rs` for the next implementer. Search capability remains disabled until its route exists. No canonical architecture/spec documents, sibling repositories, AQ lifecycle state, or external systems were modified.

Commit: see the commit containing this report (the hash cannot be included in its own content).
