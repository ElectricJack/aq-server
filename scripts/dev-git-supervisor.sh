#!/bin/sh
# One authority owns this private state/socket. The child PID is diagnostic only.
set -eu
child=''
shutdown() {
    trap '' TERM INT
    if [ -n "$child" ]; then
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" || true
        kill -KILL "-$child" 2>/dev/null || true
    fi
    exit 0
}
trap shutdown TERM INT
while :; do
    # Keep the guard and its engine in a separate process group. If the guard
    # itself dies, no orphaned engine may keep serving during a restart loop.
    setsid aq-git --state-dir /var/lib/aq/git --git-executable /opt/aq-git-runtime/bin/git --listen /run/aq-git/central.sock serve &
    child=$!
    printf '%s\n' "$child" > /run/aq-git/child.pid
    wait "$child" || true
    kill -TERM "-$child" 2>/dev/null || true
    kill -KILL "-$child" 2>/dev/null || true
    child=''
    # A short bounded delay makes outages observable and avoids a hot crash loop.
    sleep 5 &
    child=$!
    wait "$child" || true
    child=''
done
