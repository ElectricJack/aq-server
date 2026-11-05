#!/bin/sh
# Initialize only this Compose project's private volumes; never host ownership.
set -eu
umask 077
for directory in /vault /run/aq-dev /run/aq-git /var/lib/aq /var/lib/aq/git /var/lib/aq/source-staging; do
    install -d -o 10001 -g 10001 -m 0700 "$directory"
done
temporary=''
dsn_source=''
trap 'if [ -n "$temporary" ]; then rm -f "$temporary"; fi; if [ -n "$dsn_source" ]; then rm -f "$dsn_source"; fi' EXIT
copy_private() {
    source=$1
    target=$2
    test ! -L "$target" || { echo 'refusing a symlink in retained development configuration' >&2; exit 1; }
    if [ -e "$target" ]; then
        cmp -s "$source" "$target" || { echo 'retained development configuration differs; preserve it and use a fresh Compose project' >&2; exit 1; }
        test "$(stat -c '%u:%g:%a' "$target")" = '10001:10001:600' || { echo 'retained development configuration ownership differs' >&2; exit 1; }
    else
        temporary=$(mktemp "$(dirname "$target")/.e0-init.XXXXXX")
        install -o 10001 -g 10001 -m 0600 "$source" "$temporary"
        ln "$temporary" "$target"
        rm -f "$temporary"
        temporary=''
    fi
}
copy_private /dev-vault/config.toml /vault/config.toml
copy_private /run/secrets/graph_token /run/aq-dev/graph_token
copy_private /run/secrets/playbook_engine_keys /run/aq-dev/playbook_engine_keys
copy_private /run/secrets/playbook_bridge_key /run/aq-dev/playbook_bridge_key
copy_private /run/secrets/terminal_protection_key /run/aq-dev/terminal_protection_key
if [ -f /dev-vault/playbooks.toml ]; then
    install -d -o 10001 -g 10001 -m 0700 /vault/settlement
    copy_private /dev-vault/playbooks.toml /vault/settlement/config.toml
    copy_private /dev-vault/playbook-artifact.json /run/aq-dev/playbook-artifact.json
fi
# DSN contains a generated hexadecimal password; it is never a command argument.
temporary=$(mktemp /run/aq-dev/.e0-dsn.XXXXXX)
printf "host=/run/aq-postgresql port=5432 user=aq_playbook dbname=agent_q sslmode=disable password='%s'\n" "$(cat /run/secrets/playbook_database_password)" > "$temporary"
chown 10001:10001 "$temporary"
dsn_source=$temporary
temporary=''
copy_private "$dsn_source" /run/aq-dev/playbook_dsn
rm -f "$dsn_source"
