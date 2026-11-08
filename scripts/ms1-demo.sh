#!/usr/bin/env bash
# MS1 demo (stark-zenith-16.2): real Claude Code in a docker-local sandbox on
# the desktop worker, against an isolated local v2 stack. See
# docs/implementation/MS1.md for what each step proves and what is missing.
#
# Usage: scripts/ms1-demo.sh COMMAND
#   up              build and start the aq-ms1 stack (ports 28080/28081/28741)
#   seed            E0 seed (scratch repo) + the two dependent MS1 graph tasks
#   node            build aq-worker, enroll once, start the node controller
#                   (docker-local backend, claude-rust template) detached
#   live            node-level live steps with a real Claude run: edit/check/
#                   publish (step 1), cancel mid-run (step 3), node restart
#                   mid-run (step 5), with memory/CPU sampling
#   server-restart  recreate aq-server while the node controller runs (step 5)
#   drill           step 6 helper: a ~3 min real Claude sandbox run; suspend
#                   Windows when it prints "suspend Windows now", then resume
#   import-login    store the owner's Claude setup-token as the newest version
#                   of credential owner-claude-subscription@1 (C5b import)
#   gaps            show the production fences that block steps 1-5 end to end
#   graph           print the two MS1 tasks, their versions and readiness
#   stats-summary PREFIX   peak memory/CPU from ms1-stats.sh CSVs
#   rebuild-server  build agent-q-server:ms1 from this checkout and recreate the server
#   media           create the private result-media root (dashboard Results view)
#   profiled        MS1 wiring: retain the base centrally, generate [intelligence] +
#                   [profiled_execution] (scripts/ms1-profiled.py), recreate the
#                   server on it, activate catalog/binding revision 2
#   run             one graph attempt through the stack: worker-side aq-git, then
#                   `aq-worker run-native` with the Claude docker-local executor
#                   (scripts/ms1-node-run.py); repeat for the next ready task
#   remote | sync   (D1) local bare remote in the aq-git volume; central fetch
#   publication    (D1) delivery evidence, source head and the remote's main
#   status | down | node-stop
#   all             up, seed, node, import-login, live, server-restart, gaps
#
# Never prints, logs or passes the Claude token in argv: the live tests read
# the 0600 token file named by AQ_MS1_CLAUDE_TOKEN_FILE and deliver it only as
# controller private material, which the sandbox runner moves into the
# CLAUDE_CODE_OAUTH_TOKEN environment of the claude process.
set -euo pipefail
SRV=$(cd "$(dirname "$0")/.." && pwd)
WORKER_SRC=${AQ_MS1_WORKER_SRC:-/home/jkern/dev/aq-platform/wt/ms1h-worker}
DASH=${AQ_MS1_DASHBOARD:-/home/jkern/dev/aq-platform/aq-dashboard}
# One stack per Compose project: AQ_MS1_PROJECT (default aq-ms1) names the
# project, its state dir (~/.local/share/$AQ_MS1_PROJECT) and its server image
# tag; AQ_MS1_ADMIN_PORT/AQ_MS1_WORKER_PORT/AQ_MS1_GRAPH_PORT its ports.
# Example (a second stack): AQ_MS1_PROJECT=aq-ms1d AQ_MS1_ADMIN_PORT=38080
#   AQ_MS1_WORKER_PORT=38081 AQ_MS1_GRAPH_PORT=38741 scripts/ms1-demo.sh up
STACK=${AQ_MS1_PROJECT:-aq-ms1}
ADMIN_PORT=${AQ_MS1_ADMIN_PORT:-28080} WORKER_PORT=${AQ_MS1_WORKER_PORT:-28081} GRAPH_PORT=${AQ_MS1_GRAPH_PORT:-28741}
BASE=${AQ_MS1_HOME:-$HOME/.local/share/$STACK}
export AQ_MS1_HOME=$BASE
EVID=$BASE/evidence
W=$BASE/worker
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/home/jkern/dev/aq-platform/target-$STACK}
export COMPOSE_PROJECT_NAME=$STACK
export AQ_DEV_ADMIN_PORT=$ADMIN_PORT AQ_DEV_WORKER_PORT=$WORKER_PORT AQ_DEV_GRAPH_PORT=$GRAPH_PORT
export AQ_DEV_SERVER_URL=http://127.0.0.1:$ADMIN_PORT AQ_DEV_WORKER_URL=http://127.0.0.1:$WORKER_PORT
export AQ_DEV_GRAPH_URL=http://127.0.0.1:$GRAPH_PORT
export AQ_MS1_NODE_PIDFILE=$BASE/worker/controller.pid
# D1 publication (default; AQ_MS1_D1=0 turns it off before the first seed).
# The scratch project uses a local bare remote (cloned from
# ElectricJack/aq-scratch into the aq-git volume), a named-ref source on main,
# delivery required by D3 and a fast-forward publication policy. The seed's
# repository designation is immutable, so a stack keeps the mode it was
# seeded with.
if [ "${AQ_MS1_D1:-1}" = 1 ]; then D1=1; export AQ_MS1_D1=1; else D1=; export AQ_MS1_D1=0; fi
LOCAL_REMOTE=/var/lib/aq/remotes/aq-scratch.git
export AQ_DEV_STATE_DIR=$BASE/state AQ_SECRETS_DIR=$BASE/secrets
# Opt-in [intelligence] tables (the Claude subscription-login credential and
# its material store). Must be present from the stack's first `up`: the dev
# vault is immutable for a Compose project.
INTELLIGENCE=${AQ_DEV_INTELLIGENCE_FILE:-$BASE/intelligence.toml}
[ -f "$INTELLIGENCE" ] && export AQ_DEV_INTELLIGENCE_FILE=$INTELLIGENCE
# ms1-profiled.py configures credential profile version 2. The first
# import-login stages the base file, which changes the registered credential
# state: rerun `profiled` with AQ_MS1_CREDENTIAL_REVISION=2 and a bumped
# revision afterwards (MS1.md, Owner verification, fresh stack).
CREDENTIAL=${AQ_MS1_CREDENTIAL:-owner-claude-subscription@2}
CREDENTIAL_REVISION=${AQ_MS1_CREDENTIAL_REVISION:-1}
# Own image tag: never retag agent-q-server:dev, which aq-v2-local runs.
export AQ_SERVER_IMAGE=${AQ_SERVER_IMAGE:-agent-q-server:${STACK#aq-}}
SANDBOX_SOCKET=${AQ_DOCKER_LOCAL_SOCKET:-/run/user/$(id -u)/aq-sandbox/podman.sock}
# I4a's claude-rust image: C3c rust-1 plus Claude Code 2.1.318 (unadmitted).
TEMPLATE_DIGEST=${AQ_MS1_TEMPLATE_DIGEST:-sha256:194e11f54fbe104734166ec219f5ac13b2e54055fae01f6c3ce264a915f172a4}
TEMPLATE_IMAGE=${AQ_MS1_TEMPLATE_IMAGE:-127.0.0.1:5138/electricjack/aq-template-unadmitted@$TEMPLATE_DIGEST}
CLI_VERSION=${AQ_MS1_CLI_VERSION:-2.1.318}
TOKEN_FILE=${AQ_MS1_CLAUDE_TOKEN_FILE:-$HOME/.config/aq-secrets/claude-oauth-token}
TASK_A=e0a20000-0000-4000-8000-000000000011
TASK_B=e0a20000-0000-4000-8000-000000000012
WORKSPACE=e0000000-0000-4000-8000-000000000001

say() { printf '\n== %s  [%s]\n' "$*" "$(date -u +%H:%M:%SZ)"; }
compose() {
  (cd "$SRV"
   eval "$(python3 scripts/dev-stack.py prepare | python3 -c 'import json,sys
for k,v in json.loads(sys.stdin.read()).items(): print(f"export {k}={v!r}")')"
   AQ_DASHBOARD_BUNDLE=$DASH/dist docker compose -f compose.yaml -f compose.dev.yaml \
     -f compose.dashboard.yaml ${AQ_MS1_COMPOSE_EXTRA:+-f "$AQ_MS1_COMPOSE_EXTRA"} "$@")
}
mkdir -p "$BASE" "$EVID" && chmod 700 "$BASE"

cmd_rebuild_server() {
  say "build $AQ_SERVER_IMAGE from $(git -C "$SRV" rev-parse --short HEAD) and recreate the server"
  export AQ_DEV_REPLACE_OPERATOR_SECTION=1
  compose build server
  compose up -d --no-build --force-recreate --wait --wait-timeout 300 server
  curl -fsS "$AQ_DEV_SERVER_URL/healthz"; echo
}

cmd_server_up() {
  # Recreate only aq-server on the current image (AQ_MS1_COMPOSE_EXTRA may add
  # an override, e.g. RUST_LOG=info,aq_refusal=debug).
  export AQ_DEV_REPLACE_OPERATOR_SECTION=1
  compose up -d --no-build --force-recreate --wait --wait-timeout 300 server
}

cmd_media() {
  # Private result-media root on the server's /var/lib/aq volume (0700, owned
  # by the server user); [result_media] itself comes from ms1-profiled.py.
  docker exec $STACK-server-1 sh -c 'mkdir -p /var/lib/aq/result-media && chmod 0700 /var/lib/aq/result-media && ls -ld /var/lib/aq/result-media' \
    || echo "media root not checked: $STACK-server-1 is not running (the root persists on its volume)"
}

cmd_profiled() {
  say "MS1 profiled execution: central base, config, catalog/binding revision 2"
  cmd_media
  (cd "$SRV" && python3 scripts/ms1-profiled.py source && python3 scripts/ms1-profiled.py configure) \
    > "$EVID/profiled-configure.json"
  export AQ_DEV_REPLACE_OPERATOR_SECTION=1
  compose up -d --no-build --force-recreate --wait --wait-timeout 300 server
  (cd "$SRV" && python3 scripts/ms1-profiled.py apply) | tee "$EVID/profiled-apply.json"
}

cmd_node_enroll() {
  # A fresh node identity for the run-native path (a wedged attempt on the old
  # node blocks its re-registration; see MS1.md). The old enrollment is kept.
  local name=${1:?name} cfg=$BASE/node/config/worker.toml
  [ -s "$cfg" ] && mv "$cfg" "$cfg.$(date -u +%Y%m%dT%H%M%SZ)"
  code=$(curl -fsS -X POST -H 'Content-Type: application/json' -H 'X-AQ-Request: 1' -d '{}' \
    "$AQ_DEV_SERVER_URL/api/enrollments" | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])')
  printf '%s\n' "$code" | "$BASE/runtime/aq-worker" --config "$cfg" enroll --server "$AQ_DEV_WORKER_URL" --name "$name"
  chmod 600 "$cfg"
}

cmd_run() {
  say "one attempt through the stack (node $(sed -n 's/^node_id = "\(.*\)"/\1/p' "$BASE/node/config/worker.toml"))"
  # Owner availability attestation for the subscription (selection max age 30 s).
  (cd "$SRV" && python3 scripts/ms1-profiled.py observe) > "$EVID/observe-last.json"
  [ -z "$D1" ] || cmd_sync > "$EVID/sync-last.json"
  (cd "$SRV" && python3 scripts/ms1-node-run.py "$@") 2>&1 | tee -a "$EVID/node-run.log"
}

cmd_up() {
  say "build and start $STACK (server source $(git -C "$SRV" rev-parse --short HEAD))"
  compose up -d --build --wait --wait-timeout 300
  curl -fsS "$AQ_DEV_SERVER_URL/healthz"; echo
}

cmd_remote() {
  # D1: a writable local bare remote inside the aq-git volume, cloned once from
  # the public scratch repository. aq-git pushes to it with file transport.
  compose exec -T aq-git sh -ec "test -d $LOCAL_REMOTE || { mkdir -p ${LOCAL_REMOTE%/*}; \
    git clone --quiet --bare https://github.com/ElectricJack/aq-scratch.git $LOCAL_REMOTE; }; \
    git --git-dir=$LOCAL_REMOTE log --oneline -3 main"
}

cmd_sync() {
  # Central fetch of the project's remote (0005 section 4): a named-ref source
  # with no recorded publication head needs an observation under 30 s old.
  (cd "$SRV" && python3 scripts/ms1-profiled.py sync)
}

cmd_seed() {
  if [ -n "$D1" ]; then
    cmd_remote
    say "E0 seed: local bare remote $LOCAL_REMOTE (from ElectricJack/aq-scratch), main"
    (cd "$SRV" && DEV_SCRATCH_REPO_URL=$LOCAL_REMOTE DEV_SCRATCH_LOCAL_REMOTE=1 \
       DEV_SCRATCH_BASE_BRANCH=main DEV_SCRATCH_CREDENTIAL_REF=none \
       python3 scripts/dev-stack.py seed) | tee "$EVID/e0-seed.json"
  else
  say "E0 seed: ElectricJack/aq-scratch main, public (credential none)"
  (cd "$SRV" && DEV_SCRATCH_REPO_URL=https://github.com/ElectricJack/aq-scratch.git \
     DEV_SCRATCH_BASE_BRANCH=main DEV_SCRATCH_CREDENTIAL_REF=none \
     python3 scripts/dev-stack.py seed) | tee "$EVID/e0-seed.json"
  fi
  # The seed's settlement step recreates the server without the dashboard overlay.
  compose up -d --no-build --wait --wait-timeout 300 server
  # The E0 placeholder task is ready too; retire it so runs take the MS1 tasks.
  (cd "$SRV" && python3 scripts/ms1-task.py --retire e0000000-0000-4000-8000-000000000004 \
     "E0 placeholder task, not part of the MS1 demo") || true
  say "two dependent graph tasks (A blocks B)"
  GRAPH_TOKEN_FILE=$AQ_SECRETS_DIR/graph_token python3 - "$TASK_A" "$TASK_B" <<'EOF' | tee "$EVID/graph-seed.json"
import json, os, sys, urllib.request, urllib.error
a, b = sys.argv[1:3]
base = os.environ["AQ_DEV_GRAPH_URL"] + "/v1/workspaces/e0000000-0000-4000-8000-000000000001"
token = open(os.environ["GRAPH_TOKEN_FILE"]).read().strip()
def call(path, body=None):
    req = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.load(r)
ops = [dict(op="create_node", id=a, kind="task", task_state="active",
            title="MS1 step 1: add a hello function to the scratch crate",
            body="In ElectricJack/aq-scratch, add `pub fn hello() -> &'static str` returning \"hello from MS1\" with a unit test; run the checks; publish.",
            acceptance_criteria="cargo test passes; change published through aq-git"),
       dict(op="create_node", id=b, kind="task", task_state="active",
            title="MS1 step 2: document hello in the README",
            body="After step 1 is published, add one README line documenting hello(); publish.",
            acceptance_criteria="README mentions hello()"),
       dict(op="add_edge", source=a, target=b, edge_type="blocks")]
try:
    m = call("/mutations", dict(idempotency_key="ms1-demo-tasks-v1", reason="MS1 demo: two dependent tasks", operations=ops))
    print(json.dumps({"change_set_id": m.get("change_set_id")}))
except urllib.error.HTTPError as e:
    print(json.dumps({"mutation_http_status": e.code, "note": "replayed or refused; current state follows"}))
EOF
  cmd_graph
}

cmd_graph() {
  GRAPH_TOKEN_FILE=$AQ_SECRETS_DIR/graph_token python3 - "$TASK_A" "$TASK_B" <<'EOF'
import json, os, sys, urllib.request
base = os.environ["AQ_DEV_GRAPH_URL"] + "/v1/workspaces/e0000000-0000-4000-8000-000000000001"
token = open(os.environ["GRAPH_TOKEN_FILE"]).read().strip()
def get(path):
    req = urllib.request.Request(base + path, headers={"Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.load(r)
ready = {i["id"] for i in get("/ready")["items"]}
for n in sys.argv[1:3]:
    x = get("/nodes/" + n); x = x.get("node", x)
    print(json.dumps(dict(id=n, title=x["title"], task_state=x["task_state"], version=x["version"], ready=n in ready)))
EOF
}

# The controller runs from a private copy, so cleaning the Cargo target dir
# never removes the binary of a running node.
worker_bin() { echo "$BASE/bin/aq-worker"; }

cmd_node() {
  say "build aq-worker $(git -C "$WORKER_SRC" rev-parse --short HEAD)"
  (cd "$WORKER_SRC" && cargo build --locked -p aq-worker --bin aq-worker)
  install -D -m 0755 "$CARGO_TARGET_DIR/debug/aq-worker" "$(worker_bin)"
  mkdir -p "$W" && chmod 700 "$W"
  if [ ! -s "$W/worker.toml" ]; then
    say "enroll node ms1-desktop"
    code=$(curl -fsS -X POST -H 'Content-Type: application/json' -H 'X-AQ-Request: 1' -d '{}' \
      "$AQ_DEV_SERVER_URL/api/enrollments" | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])')
    printf '%s\n' "$code" | "$(worker_bin)" --config "$W/worker.toml" enroll --server "$AQ_DEV_WORKER_URL" --name ms1-desktop
    unset code
  fi
  # The controller creates its own 0700 root; never pre-create it.
  cat > "$W/node.json" <<EOF
{
  "state_dir": "$W/controller",
  "docker_local": {"socket": "$SANDBOX_SOCKET", "shared_root_fs": true,
    "templates": [{"image": "$TEMPLATE_IMAGE", "digest": "$TEMPLATE_DIGEST"}]},
  "node": {
    "runtime": {"id": "aq-worker", "version": "0.5.1", "digest": "0000000000000000000000000000000000000000000000000000000000000000"},
    "capabilities": [], "features": [],
    "templates": [{"digest": "${TEMPLATE_DIGEST#sha256:}",
      "harnesses": [{"harness": "claude-code", "version": "$CLI_VERSION"}],
      "estimated_cold_start_ms": 30000, "estimated_cold_start_cost": {"currency": "USD", "micros": 0}}],
    "max_concurrent_sandboxes": 2, "spend_ceiling": null,
    "capacity": {"memory_bytes": 8589934592, "cpu_millis": 4000, "disk_bytes": 53687091200, "max_preparing": 1}
  }
}
EOF
  host_load=null
  if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null; then host_load='"/mnt/c/ProgramData/agent-q/host-load.json"'; fi
  cat > "$W/machine.json" <<EOF
{"key": "desktop", "label": "desktop", "kind": "local", "host_load_file": $host_load}
EOF
  chmod 600 "$W/node.json"
  cmd_node_stop
  say "start node controller (detached; log $W/controller.log)"
  (cd "$W" || exit 1
   RUST_LOG=info setsid nohup "$(worker_bin)" --config "$W/worker.toml" run \
     --controller-config "$W/node.json" >> "$W/controller.log" 2>&1 < /dev/null &
   echo $! > "$W/controller.pid")
  sleep 15
  sed 's/\x1b\[[0-9;]*m//g' "$W/controller.log" | tail -4
  cmd_status
}

cmd_node_stop() {
  if [ -s "$W/controller.pid" ] && kill -0 "$(cat "$W/controller.pid")" 2>/dev/null; then
    kill -TERM "$(cat "$W/controller.pid")"; sleep 3
  fi
  rm -f "$W/controller.pid"
}

live_env() {
  export AQ_DOCKER_LOCAL_SOCKET=$SANDBOX_SOCKET AQ_CLAUDE_LIVE_IMAGE=$TEMPLATE_IMAGE
  export AQ_CLAUDE_LIVE_CLI_VERSION=$CLI_VERSION AQ_CLAUDE_LIVE_TOKEN_FILE=$TOKEN_FILE
  export AQ_DOCKER_LOCAL_SHARED_ROOT_FS=1 RUST_TEST_THREADS=1
}

cmd_live() {
  live_env
  run=$EVID/run-$(date -u +%Y%m%dT%H%M%SZ); mkdir -p "$run"
  "$SRV/scripts/ms1-stats.sh" "$run/stats" & stats=$!
  trap 'kill $stats 2>/dev/null || true' RETURN EXIT
  docker -H "unix://$SANDBOX_SOCKET" image inspect "$TEMPLATE_IMAGE" --format '{{.Id}}' > "$run/template-image-id.txt"
  say "step 1 (node level): real Claude edit, check and publish in a docker-local sandbox"
  (cd "$WORKER_SRC" && AQ_CLAUDE_LIVE_EVIDENCE_DIR=$run/step1 timeout 1500 cargo test --locked -p aq-worker \
     --test claude_live live -- --ignored --nocapture --skip ms1_live:: --skip sandbox_driver_live::) 2>&1 | tee "$run/step1.log" | grep -E "runner exit|test result"
  say "steps 3 and 5 (node level): cancel mid-run, node-controller restart mid-run"
  (cd "$WORKER_SRC" && AQ_CLAUDE_LIVE_EVIDENCE_DIR=$run/node timeout 1500 cargo test --locked -p aq-worker \
     --test claude_live ms1_live::ms1_ -- --ignored --nocapture --test-threads=1) 2>&1 | tee "$run/node.log" \
     | grep -E "^(cancel|node-restart).json|test result"
  sleep 6
  cmd_stats_summary "$run/stats"
  echo "evidence: $run"
}

cmd_drill() {
  live_env
  run=$EVID/drill-$(date -u +%Y%m%dT%H%M%SZ); mkdir -p "$run"
  "$SRV/scripts/ms1-stats.sh" "$run/stats" & stats=$!
  say "step 6 helper: suspend Windows (Start > Power > Sleep) after the prompt, resume after >= 60 s"
  (cd "$WORKER_SRC" && AQ_CLAUDE_LIVE_EVIDENCE_DIR=$run timeout 2400 cargo test --locked -p aq-worker \
     --test claude_live ms1_live::ms1_suspend_drill -- --ignored --nocapture) 2>&1 | tee "$run/drill.log" \
     | grep --line-buffered -E "^drill:|suspend-drill.json|test result" || true
  kill $stats 2>/dev/null || true
  sed 's/\x1b\[[0-9;]*m//g' "$W/controller.log" > "$run/controller.log" 2>/dev/null || true
  docker logs --since 30m $STACK-server-1 > "$run/server.log" 2>&1 || true
  echo "evidence: $run"
}

cmd_server_restart() {
  say "recreate aq-server while the node controller runs"
  [ -s "$W/controller.pid" ] || { echo "node controller not running; run: $0 node" >&2; return 1; }
  pid=$(cat "$W/controller.pid"); before=$(date -u +%FT%TZ)
  compose up -d --no-build --force-recreate --wait --wait-timeout 300 server
  after=$(date -u +%FT%TZ); sleep 20
  echo "server recreated between $before and $after"
  echo "controller pid $pid still running: $(kill -0 "$pid" 2>/dev/null && echo yes || echo no)"
  sed 's/\x1b\[[0-9;]*m//g' "$W/controller.log" | awk -v t="$before" '$1 ~ /^20/ && $1 >= t' | tail -6
  cmd_status
}

stored_versions() {
  compose exec -T postgres sh -ec "PGPASSWORD=\"\$(cat /run/secrets/postgres_password)\" psql -h 127.0.0.1 -U agent_q -d agent_q -tAc \"SELECT count(*) FROM credential_material_revisions WHERE credential_identity='$CREDENTIAL'\""
}

cmd_import_login() {
  say "store the Claude setup-token as credential $CREDENTIAL (revision $CREDENTIAL_REVISION)"
  [ -f "$TOKEN_FILE" ] && [ ! -L "$TOKEN_FILE" ] && [ "$(stat -c %a "$TOKEN_FILE")" = 600 ] \
    || { echo "token file must be a 0600 regular file" >&2; return 1; }
  base=/var/lib/aq/credentials/owner-claude-subscription
  staged=/var/lib/aq/.ms1-login-import
  # The token travels on stdin only, never argv. The raw file is piped as is:
  # the server trims a Claude token's trailing CR/LF (import and version 0).
  if ! compose exec -T server test -s "$base"; then
    echo "C5b needs a configured base file (version 0): staging it, then restarting the server"
    compose exec -T server sh -ec \
      "umask 077; mkdir -p ${base%/*}; cat > $base.tmp; mv $base.tmp $base" < "$TOKEN_FILE"
    compose up -d --no-build --force-recreate --wait --wait-timeout 300 server
  fi
  [ "${1:-}" = --replace ] || [ "$(stored_versions)" = 0 ] || { echo "a stored version exists; pass --replace to rotate (never mid-attempt: running attempts get 409 at renewal)"; return 0; }
  compose exec -T server sh -ec "umask 077; cat > $staged" < "$TOKEN_FILE"
  compose exec -T server sh -ec "trap 'rm -f $staged' EXIT
    export PGPASSWORD=\"\$(cat /run/secrets/postgres_password)\" DATABASE_URL=postgres://agent_q@postgres:5432/agent_q
    vault=/vault; if test -f /vault/settlement/config.toml; then vault=/vault/settlement; fi
    aq-server import-login --vault \"\$vault\" --credential $CREDENTIAL --revision $CREDENTIAL_REVISION --source $staged" 2>&1 \
    | sed 's/\x1b\[[0-9;]*m//g' | grep -v 'INFO' | sed 's/^/import-login: /'
  compose exec -T server sh -ec "test ! -e $staged && echo 'staged copy removed'; ls -ld /var/lib/aq/login-material | cut -c1-10"
  echo "stored versions of $CREDENTIAL: $(stored_versions)"
}

cmd_gaps() {
  say "production fences that block MS1 steps 1-5 through the stack"
  echo "node controller (aq-worker run):"
  sed 's/\x1b\[[0-9;]*m//g' "$W/controller.log" 2>/dev/null | grep -E "admission fenced" | tail -1 || true
  grep -n "UnavailableProvider)" "$WORKER_SRC/crates/aq-worker/src/main.rs" || true
  echo "in-sandbox runner (aq-worker sandbox-run):"
  grep -n "receipt codec is unavailable" "$WORKER_SRC/crates/aq-worker/src/main.rs" || true
  echo "server v2 execution offers:"
  grep -n "synthetic assignments are development-only" "$SRV/crates/aq-server/src/execution/api.rs" || true
  say "ready graph tasks (A stays ready and unclaimed while admission is fenced)"
  cmd_graph
}

cmd_status() {
  docker ps --filter label=com.docker.compose.project=$STACK --format '{{.Names}}\t{{.Status}}\t{{.Ports}}'
  curl -fsS "$AQ_DEV_SERVER_URL/api/workers" | python3 -c 'import json,sys
for w in json.load(sys.stdin): print("worker", w["name"], w["id"], w["status"], w.get("last_seen_at"))'
  echo "sandboxes on $SANDBOX_SOCKET: $(docker -H "unix://$SANDBOX_SOCKET" ps -aq | wc -l)"
  echo "dashboard: $AQ_DEV_SERVER_URL/   worker API: $AQ_DEV_WORKER_URL   graph: $AQ_DEV_GRAPH_URL"
}

cmd_stats_summary() {
  python3 - "$1" <<'EOF'
import csv, re, sys, collections
p = sys.argv[1]
def mib(s):
    v, u = re.match(r"([\d.]+)\s*([KMG]i?B|B)", s.strip()).groups(); v = float(v)
    return v * {"B": 1/2**20, "KiB": 1/1024, "KB": 1/1024, "MiB": 1, "MB": 1, "GiB": 1024, "GB": 1024}[u]
peak, cpu = collections.defaultdict(float), collections.defaultdict(float)
snap = collections.defaultdict(float)
try:
    for r in csv.DictReader(open(p + "-stack.csv")):
        m = mib(r["mem_usage"].split("/")[0]); n = r["name"]
        peak[n] = max(peak[n], m); cpu[n] = max(cpu[n], float(r["cpu_pct"].rstrip("%") or 0)); snap[r["utc"]] += m
    for n in sorted(peak): print(f"stack  {n:28s} peak {peak[n]:7.1f} MiB  max cpu {cpu[n]:6.1f}%")
    if snap: print(f"stack  {'(sum of services)':28s} peak {max(snap.values()):7.1f} MiB")
except FileNotFoundError: pass
rows = collections.defaultdict(list)
try:
    for r in csv.DictReader(open(p + "-sandbox.csv")): rows[r["container"]].append(r)
except FileNotFoundError: pass
from datetime import datetime
for c, rs in rows.items():
    pk = max(int(r["memory_peak_bytes"] or r["memory_current_bytes"]) for r in rs) / 2**20
    t = [datetime.strptime(r["utc"], "%Y-%m-%dT%H:%M:%SZ") for r in rs]
    u = [int(r["cpu_usage_usec"] or 0) for r in rs]
    span = (t[-1] - t[0]).total_seconds() or 1
    print(f"sandbox {c} samples {len(rs):3d} peak {pk:7.1f} MiB  avg cpu {100*(u[-1]-u[0])/1e6/span:6.1f}% of one core over {span:.0f}s")
try:
    rss = [int(r["rss_kib"]) for r in csv.DictReader(open(p + "-node.csv"))]
    if rss: print(f"node controller RSS peak {max(rss)/1024:.1f} MiB")
except FileNotFoundError: pass
EOF
}

case ${1:-} in
  up) cmd_up ;;
  seed) cmd_seed ;;
  node) cmd_node ;;
  node-stop) cmd_node_stop ;;
  live) cmd_live ;;
  server-restart) cmd_server_restart ;;
  drill) cmd_drill ;;
  import-login) cmd_import_login "${2:-}" ;;
  gaps) cmd_gaps ;;
  graph) cmd_graph ;;
  status) cmd_status ;;
  rebuild-server) cmd_rebuild_server ;;
  profiled) cmd_profiled ;;
  media) cmd_media ;;
  server-up) cmd_server_up ;;
  run) shift; cmd_run "$@" ;;
  node-enroll) cmd_node_enroll "${2:-}" ;;
  stats-summary) cmd_stats_summary "${2:?prefix}" ;;
  remote) cmd_remote ;;
  sync) cmd_sync ;;
  publication) (cd "$SRV" && python3 scripts/ms1-profiled.py publication) ;;
  down) cmd_node_stop; compose down ;;
  all) cmd_up; cmd_seed; cmd_node; cmd_import_login; cmd_live; cmd_server_restart; cmd_gaps ;;
  *) sed -n '2,34p' "$0"; exit 2 ;;
esac
