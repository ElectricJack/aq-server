#!/usr/bin/env bash
set -euo pipefail
repo=/home/jkern/dev/agent-q-aq21-migration
out="$repo/target/aq21-migration-evidence"
name="aq21-migration-${UID}-$$-${RANDOM}"
image='postgres@sha256:266d3c257594e195b309ae9d1805e54c788f1d7b375f56bf6c367aac6c917e7b'
cleanup(){ docker logs "$name" > "$out/postgres.log" 2>&1 || true; docker rm --force --volumes "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM
cd "$repo"
docker run --detach --rm --name "$name" --label aq.qualification=AQ21-migration12 --cpus=2 --memory=1g --shm-size=128m -e POSTGRES_PASSWORD=synthetic-aq21 -p 127.0.0.1::5432 "$image" -c fsync=on -c synchronous_commit=on -c full_page_writes=on > "$out/container.id"
for attempt in {1..100}; do if docker exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; then break; fi; if [[ "$attempt" == 100 ]]; then exit 1; fi; sleep .2; done
port="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' "$name")"
export DATABASE_URL="postgres://postgres:synthetic-aq21@127.0.0.1:$port/postgres"
# Shared build cache only; Cargo source paths are the separate migration-only
# worktree. No runtime/core worktree files belong to this manifest/workspace.
export CARGO_TARGET_DIR=/home/jkern/dev/agent-q-aq21/target
CARGO_BUILD_JOBS=3 cargo test -p aq-server --test session_inspection --locked -- --test-threads=2 > "$out/tests.log" 2>&1
CARGO_BUILD_JOBS=3 cargo test -p aq-server --lib state_format_is_the_latest_migration --locked > "$out/state-format.log" 2>&1
CARGO_BUILD_JOBS=3 cargo clippy -p aq-server --test session_inspection --locked -- -D warnings > "$out/clippy.log" 2>&1
cargo fmt --all -- --check > "$out/format.log" 2>&1
printf '0\n' > "$out/exit-code.txt"
