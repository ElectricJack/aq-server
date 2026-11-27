---
name: postgres17-terminal-test
default: false
provides: [postgres, terminal-test]
---

## Install

- sh -ec 'set -eu; . /etc/os-release; test "$ID:$VERSION_CODENAME" = debian:bookworm; apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl; curl --fail --silent --show-error --location https://www.postgresql.org/media/keys/ACCC4CF8.asc -o /usr/share/keyrings/aq-pgdg.asc; printf "%s  %s\n" 0144068502a1eddd2a0280ede10ef607d1ec592ce819940991203941564e8e76 /usr/share/keyrings/aq-pgdg.asc | sha256sum --check --status; echo "deb [signed-by=/usr/share/keyrings/aq-pgdg.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main 17" > /etc/apt/sources.list.d/aq-pgdg.list; echo "deb [signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] https://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/aq-bookworm-backports.list; apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd-standalone-sysusers=254.26-1~bpo12+1 systemd-standalone-tmpfiles=254.26-1~bpo12+1; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends postgresql-common=297.pgdg12+1; printf "\ncreate_main_cluster = false\n" >> /etc/postgresql-common/createcluster.conf; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends postgresql-17=17.12-1.pgdg12+1 postgresql-client-17=17.12-1.pgdg12+1 libpq5=17.12-1.pgdg12+1; ln -sf /usr/lib/postgresql/17/bin/pg_config /usr/local/bin/pg_config; rm -f /etc/ssl/private/ssl-cert-snakeoil.key /etc/ssl/certs/ssl-cert-snakeoil.pem; apt-get clean; rm -rf /var/lib/apt/lists/*'
- sh -ec 'set -eu; apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends python3=3.11.2-1+b1 xz-utils=5.4.1-1+deb12u2; stage=$(mktemp -d); trap "rm -rf \"$stage\"" EXIT; cd "$stage"; curl -4fLsS https://nodejs.org/dist/v24.11.0/node-v24.11.0-linux-x64.tar.xz -o node.tar.xz; curl -4fLsS https://registry.npmjs.org/pnpm/-/pnpm-10.34.5.tgz -o pnpm.tgz; printf "%s  %s\n" 46da9a098973ab7ba4fca76945581ecb2eaf468de347173897044382f10e0a0a node.tar.xz ccb5c479cab1b00621325bfe7d4c9a8a8031e7a525d7249e275ecbec81b08db2 pnpm.tgz | sha256sum -c -; mkdir /opt/node /opt/pnpm; tar -xJf node.tar.xz -C /opt/node --strip-components=1; tar -xzf pnpm.tgz -C /opt/pnpm --strip-components=1; for tool in node npm npx; do ln -s "/opt/node/bin/$tool" "/usr/local/bin/$tool"; done; ln -s /opt/pnpm/bin/pnpm.cjs /usr/local/bin/pnpm; apt-get clean; rm -rf /var/lib/apt/lists/*'

## Check

- test "$(id -u agent)" = 10002
- test ! -d /var/lib/postgresql/17/main
- test ! -e /etc/ssl/private/ssl-cert-snakeoil.key
- test ! -e /usr/lib/systemd/systemd
- test "$(pg_config --bindir)" = /usr/lib/postgresql/17/bin
- /usr/lib/postgresql/17/bin/postgres --version | grep -F " 17.12"
- /usr/sbin/runuser -u agent -- sh -ec 'stage=$(mktemp -d); trap "rm -rf \"$stage\"" EXIT; /usr/lib/postgresql/17/bin/initdb --auth=trust --no-sync --locale=C -D "$stage/db" >/dev/null'
- /usr/sbin/runuser -u agent -- env -i PATH=/usr/local/bin:/usr/bin:/bin sh -ec 'test "$(command -v node)" = /usr/local/bin/node; test "$(node --version)" = v24.11.0; test "$(pnpm --version)" = 10.34.5; python3 --version | grep -F "Python 3.11."'
