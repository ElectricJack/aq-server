---
name: postgres17-canary
default: false
provides: [postgres]
---

## Install

- sh -ec 'set -eu; . /etc/os-release; test "$ID:$VERSION_CODENAME" = debian:bookworm; apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl; curl --fail --silent --show-error --location https://www.postgresql.org/media/keys/ACCC4CF8.asc -o /usr/share/keyrings/aq-pgdg.asc; printf "%s  %s\n" 0144068502a1eddd2a0280ede10ef607d1ec592ce819940991203941564e8e76 /usr/share/keyrings/aq-pgdg.asc | sha256sum --check --status; echo "deb [signed-by=/usr/share/keyrings/aq-pgdg.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main 17" > /etc/apt/sources.list.d/aq-pgdg.list; echo "deb [signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] https://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/aq-bookworm-backports.list; apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd-standalone-sysusers=254.26-1~bpo12+1 systemd-standalone-tmpfiles=254.26-1~bpo12+1; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends postgresql-common=297.pgdg12+1; printf "\ncreate_main_cluster = false\n" >> /etc/postgresql-common/createcluster.conf; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends postgresql-17=17.12-1.pgdg12+1 postgresql-client-17=17.12-1.pgdg12+1 libpq5=17.12-1.pgdg12+1; ln -sf /usr/lib/postgresql/17/bin/pg_config /usr/local/bin/pg_config; rm -f /etc/ssl/private/ssl-cert-snakeoil.key /etc/ssl/certs/ssl-cert-snakeoil.pem; apt-get clean; rm -rf /var/lib/apt/lists/*'

## Check

- test "$(id -u agent)" = 10002
- test ! -d /var/lib/postgresql/17/main
- test ! -e /etc/ssl/private/ssl-cert-snakeoil.key
- test ! -e /usr/lib/systemd/systemd
- test "$(pg_config --bindir)" = /usr/lib/postgresql/17/bin
- /usr/lib/postgresql/17/bin/postgres --version | grep -F " 17.12"
- /usr/sbin/runuser -u agent -- sh -ec 'stage=$(mktemp -d); trap "rm -rf \"$stage\"" EXIT; /usr/lib/postgresql/17/bin/initdb --auth=trust --no-sync --locale=C -D "$stage/db" >/dev/null'
