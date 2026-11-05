#!/bin/sh
# Initialize only the worker's named volumes for the sandbox socket owner's UID.
set -eu
for value in "$AQ_SANDBOX_UID" "$AQ_SANDBOX_GID"; do
    case "$value" in ''|*[!0-9]*) echo 'sandbox UID/GID must be numeric' >&2; exit 1 ;; esac
    test "$value" -gt 0 || { echo 'sandbox UID/GID must be non-root' >&2; exit 1; }
done
for directory in /state /var/lib/aq-worker/leases; do
    install -d -o "$AQ_SANDBOX_UID" -g "$AQ_SANDBOX_GID" -m 0700 "$directory"
done
