#!/bin/sh
# Only project-owned volumes; never change ownership of host bind mounts.
set -eu
# The updater supplies AQ_SERVER_IMAGE unchanged; validate before state is used.
case "$AQ_SERVER_IMAGE" in
    *@sha256:*) digest=${AQ_SERVER_IMAGE##*@sha256:} ;;
    *) echo 'AQ_SERVER_IMAGE must be digest-pinned' >&2; exit 1 ;;
esac
case "$digest" in *[!0-9a-f]*) echo 'invalid image digest' >&2; exit 1 ;; esac
test "${#digest}" = 64 || { echo 'invalid image digest length' >&2; exit 1; }
umask 077
for directory in /run/aq /run/aq-git /var/lib/aq /var/lib/aq/git /var/lib/aq/source-staging; do
    install -d -o 10001 -g 10001 -m 0700 "$directory"
done
target=/run/aq/graph_token
test ! -L "$target" || { echo 'refusing symlink in retained production credentials' >&2; exit 1; }
if [ -e "$target" ]; then
    cmp -s /run/secrets/graph_token "$target" || { echo 'retained graph token differs; rotate explicitly' >&2; exit 1; }
    test "$(stat -c '%u:%g:%a' "$target")" = '10001:10001:600' || { echo 'retained graph token ownership differs' >&2; exit 1; }
else
    install -o 10001 -g 10001 -m 0600 /run/secrets/graph_token "$target"
fi
