#!/bin/sh
# The placeholder performs no enrollment, provider calls or sandbox creation.
set -eu
umask 077
if [ "${AQ_BOAT_ENABLED:-0}" != 1 ] ||
   [ ! -s /config/controller.json ] ||
   [ ! -s /config/agent-q/worker.toml ] ||
   [ ! -s /run/secrets/boat_attempt_key ]; then
    printf '%s\n' configuration-required > /run/aq-boat/status
    printf '%s\n' 'aq-worker-boat: configuration-required; see docs/operations.md'
    exec sleep infinity
fi
# The qualified cloud image predates Boat; never silently use it for attempts.
test -n "${AQ_BOAT_WORKER_IMAGE:-}" || {
    echo 'aq-worker-boat: set AQ_BOAT_WORKER_IMAGE to a C7-capable digest' >&2
    exit 1
}
# BoatKey requires a same-owner private regular file. Compose file-backed
# secrets retain host ownership/mode, so copy to this node-only tmpfs first.
cp /run/secrets/boat_attempt_key /run/aq-boat/api-key
chmod 0600 /run/aq-boat/api-key
printf '%s\n' controller-starting > /run/aq-boat/status
exec aq-worker run --controller-config /config/controller.json
