#!/usr/bin/env bash
# MS1: sample memory and CPU until killed.
#   PREFIX-stack.csv    the aq-ms1 Compose services (host Docker), every 5 s
#   PREFIX-sandbox.csv  every sandbox container on the dedicated rootless
#                       sandbox daemon, read from its cgroup every second
#                       (memory.current, memory.peak, cumulative CPU)
#   PREFIX-node.csv     the node controller process RSS, every 5 s
# Usage: scripts/ms1-stats.sh PREFIX
# Summarize with: scripts/local-stack stats-summary PREFIX
set -uo pipefail
prefix=${1:?usage: ms1-stats.sh PREFIX}
project=${COMPOSE_PROJECT_NAME:-aq-ms1}
cgroups=${AQ_SANDBOX_CGROUP_ROOT:-/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/user.slice}
pidfile=${AQ_MS1_NODE_PIDFILE:-$HOME/.local/share/aq-ms1/worker/controller.pid}
[ -s "$prefix-stack.csv" ] || echo 'utc,name,cpu_pct,mem_usage,mem_pct' > "$prefix-stack.csv"
[ -s "$prefix-sandbox.csv" ] || echo 'utc,container,memory_current_bytes,memory_peak_bytes,cpu_usage_usec' > "$prefix-sandbox.csv"
[ -s "$prefix-node.csv" ] || echo 'utc,pid,rss_kib' > "$prefix-node.csv"
(
  while :; do
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    for d in "$cgroups"/libpod-*.scope; do
      case $d in *libpod-conmon-*|*'libpod-*.scope') continue ;; esac
      id=${d##*/libpod-}; id=${id%.scope}
      cur=$(cat "$d/memory.current" 2>/dev/null) || continue
      peak=$(cat "$d/memory.peak" 2>/dev/null || echo)
      cpu=$(awk '/^usage_usec/ {print $2}' "$d/cpu.stat" 2>/dev/null)
      echo "$ts,${id:0:12},$cur,$peak,$cpu"
    done >> "$prefix-sandbox.csv"
    sleep 1
  done
) &
fast=$!
trap 'kill $fast 2>/dev/null' EXIT
# A plain TERM handler would return to the loop and leave the sampler running.
trap 'exit 0' INT TERM
while :; do
  ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  ids=$(docker ps -q --filter "label=com.docker.compose.project=$project")
  [ -n "$ids" ] && docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}},{{.MemPerc}}' $ids \
    | sed "s|^|$ts,|" >> "$prefix-stack.csv"
  if [ -s "$pidfile" ] && pid=$(cat "$pidfile") && [ -r "/proc/$pid/status" ]; then
    echo "$ts,$pid,$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status")" >> "$prefix-node.csv"
  fi
  sleep 5
done
