#!/usr/bin/env bash
set -euo pipefail
# PB-02: pinned disposable PostgreSQL only. This script never accepts a database
# URL, existing container or persistent volume as a destination.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root"
output_dir="${1:-$repo_root/.local/pb02/$(date -u +%Y%m%dT%H%M%SZ)}"
profile="${2:-full}"
if [[ "$profile" != full && "$profile" != minimal ]]; then echo "profile must be full or minimal" >&2; exit 2; fi
mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
if [[ -e "$output_dir/results.jsonl" ]]; then echo 'Refusing to overwrite retained benchmark results' >&2; exit 2; fi
image='postgres@sha256:266d3c257594e195b309ae9d1805e54c788f1d7b375f56bf6c367aac6c917e7b'
container_name="aq-pb02-${UID}-$$-${RANDOM}"
token="pb02-disposable-$$-${RANDOM}-${RANDOM}"
cleanup() {
  docker logs "$container_name" > "$output_dir/database.log" 2>&1 || true
  docker rm --force --volumes "$container_name" >/dev/null 2>&1 || true
  (cd "$output_dir" && find . -maxdepth 1 -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
}
trap cleanup EXIT INT TERM
{
  date -u --iso-8601=seconds
  uname -a
  rustc -Vv
  cargo -V
  git rev-parse HEAD
  git status --short
  cat /proc/loadavg
  docker info --format '{{.OperatingSystem}} CPUs={{.NCPU}} Memory={{.MemTotal}}'
} > "$output_dir/environment.txt"
lscpu --json > "$output_dir/lscpu.json"
python3 - "$output_dir/source-inventory.json" <<'PY'
import pathlib,hashlib,json,sys,subprocess
files=[pathlib.Path('Cargo.toml'),pathlib.Path('Cargo.lock')]+[p for p in pathlib.Path('crates').rglob('*') if p.is_file()]
json.dump({'base_commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'files':{str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(files)}},open(sys.argv[1],'w'),indent=2)
PY
CARGO_BUILD_JOBS=3 cargo build -p aq-playbooks-postgres --example operating_envelope --release --locked > "$output_dir/build.log" 2>&1
sha256sum target/release/examples/operating_envelope > "$output_dir/binary.sha256"
target/release/examples/operating_envelope --check-fixtures > "$output_dir/fixture-check.json"
docker run --detach --rm --name "$container_name" --label aq.qualification=PB-02 \
  --cpus=2 --memory=1g --shm-size=128m \
  -e POSTGRES_PASSWORD=synthetic-pb02-password -e POSTGRES_DB=pb02_disposable \
  -p 127.0.0.1::5432 "$image" \
  -c shared_buffers=128MB -c fsync=on -c synchronous_commit=on \
  -c full_page_writes=on -c max_connections=32 > "$output_dir/container.id"
for attempt in {1..80}; do
  if docker exec "$container_name" pg_isready -h 127.0.0.1 -U postgres -d pb02_disposable >/dev/null 2>&1; then break; fi
  if [[ "$attempt" == 80 ]]; then docker logs "$container_name" > "$output_dir/database.log"; exit 1; fi
  sleep 0.25
done
port="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' "$container_name")"
docker inspect --format '{{json .HostConfig}}' "$container_name" > "$output_dir/container-host-config.json"
docker exec "$container_name" psql -U postgres -d pb02_disposable -v ON_ERROR_STOP=1 -c "CREATE TABLE public.aq_pb02_disposable(token text PRIMARY KEY); INSERT INTO public.aq_pb02_disposable VALUES ('$token');" > "$output_dir/disposable-marker.log"
docker exec "$container_name" psql -U postgres -d pb02_disposable -Atc "SELECT json_build_object('version',version(),'fsync',current_setting('fsync'),'synchronous_commit',current_setting('synchronous_commit'),'full_page_writes',current_setting('full_page_writes'),'shared_buffers',current_setting('shared_buffers'),'max_connections',current_setting('max_connections'));" > "$output_dir/postgres.json"
export AQ_PLAYBOOKS_TEST_DATABASE_URL="host=127.0.0.1 port=$port user=postgres password=synthetic-pb02-password dbname=pb02_disposable"
export AQ_PB02_DISPOSABLE_TOKEN="$token"
benchmark_args=()
if [[ "$profile" == minimal ]]; then benchmark_args+=(--minimal); fi
printf '%s\n' "$profile" > "$output_dir/profile.txt"
set +e
/usr/bin/time -v -o "$output_dir/process-time.txt" timeout 900 target/release/examples/operating_envelope "${benchmark_args[@]}" > "$output_dir/results.jsonl" 2> "$output_dir/benchmark.stderr"
status=$?
set -e
printf '%s\n' "$status" > "$output_dir/exit-code.txt"
docker exec "$container_name" psql -U postgres -d pb02_disposable -Atc "SELECT json_build_object('database_bytes',pg_database_size(current_database()),'transactions_committed',xact_commit,'transactions_rolled_back',xact_rollback,'block_reads',blks_read,'block_hits',blks_hit,'temporary_bytes',temp_bytes) FROM pg_stat_database WHERE datname=current_database();" > "$output_dir/database-cost.json"
docker stats --no-stream --format '{{json .}}' "$container_name" > "$output_dir/database-resources.json"
cat /proc/loadavg > "$output_dir/loadavg-after.txt"
python3 - "$output_dir" <<'PY'
import pathlib,hashlib,json,sys
root=pathlib.Path(sys.argv[1]); hashes={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(root.iterdir()) if p.is_file()}
(root/'SHA256SUMS').write_text(''.join(f'{digest}  {name}\n' for name,digest in hashes.items()))
PY
printf 'PB-02 results retained at %s (exit %s)\n' "$output_dir" "$status"
exit "$status"
