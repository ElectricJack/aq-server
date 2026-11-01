#!/usr/bin/env bash
# Recorded final checks. Dedicated local disposable PostgreSQL only.
set -euo pipefail
export DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres
export RUST_TEST_THREADS=2 CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR=/home/jkern/dev/agent-q/target
cargo test -p aq-server --lib --test native_initial_grant --test native_offers --test native_preparation --test intelligence --test profiled_hot --test profiled_authority --test profiled_started --test profiled_stopped --test never_granted --test project_bindings --test schema26_27 --test dashboard_read --test playbook_review_contract --locked --offline
cargo clippy -p aq-server --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all -- --check
