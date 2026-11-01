#!/usr/bin/env bash
set -euo pipefail
# Run from any directory. All state is disposable; no production database or
# persistent volume is touched. Override the image for an approved local mirror.
crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
container_name="aq-playbooks-test-${UID}-${RANDOM}-$$"
cleanup() { docker rm --force "$container_name" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM
docker run --detach --rm --name "$container_name" \
  -e POSTGRES_PASSWORD=synthetic-test-password -e POSTGRES_DB=playbooks_test \
  -p 127.0.0.1::5432 "${AQ_PLAYBOOKS_POSTGRES_IMAGE:-postgres:17-alpine}" >/dev/null
for attempt in {1..60}; do
  if docker exec "$container_name" pg_isready -U postgres -d playbooks_test >/dev/null 2>&1; then break; fi
  if [[ "$attempt" == 60 ]]; then docker logs "$container_name"; exit 1; fi
  sleep 0.25
done
port="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' "$container_name")"
export AQ_PLAYBOOKS_TEST_DATABASE_URL="host=127.0.0.1 port=$port user=postgres password=synthetic-test-password dbname=playbooks_test"
cd "$crate_dir/../.."
cargo test -p aq-playbooks-postgres --features postgres-tests "$@"
if [[ "$#" == 0 ]]; then
  cargo test -p aq-playbooks-cli --features postgres-tests --test admin_cli
fi
