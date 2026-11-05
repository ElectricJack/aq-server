# Operations

How releases are built and promoted, how the host updater (`aq-updater`) is
installed and run, and what to do when an update needs an operator.

The provisioning of the real host (Bluehost VPS, Cloudflare Tunnel and
Access) and the live demonstrations of self-update belong to the
hosted-access plan; see [Not covered here](#not-covered-here).

## The pieces

- **Images** on GHCR, always referenced by digest:
  `ghcr.io/electricjack/agent-q-server`, `ghcr.io/electricjack/agent-q-worker`.
- **Release image** `ghcr.io/electricjack/agent-q-release:<version>`: a
  `FROM scratch` image holding only `manifest.json` (version, source revision,
  the two image digests, protocol range, state formats, upgrade range) and
  `manifest.sigstore.json`, its cosign bundle.
- **Channel** `ghcr.io/electricjack/agent-q-release:stable`: the release image
  that `promote.yml` last pointed it at.
- **`aq-updater`**, a host service outside the containers and the only thing
  that runs `docker`. It installs a requested release into the fixed Compose
  project, rolls back on failure, and recovers after a crash or reboot.
- **The server** never gets the Docker socket. It writes update requests into
  the spool and reads the updater's `status.json` and `maintenance` marker
  from it.

Trust: the updater accepts a release only if `cosign verify-blob` accepts its
manifest bundle for this identity and issuer, bound to the tag and commit the
manifest itself names:

```
--certificate-identity-regexp '^https://github\.com/ElectricJack/aq-server/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$'
--certificate-oidc-issuer https://token.actions.githubusercontent.com
--certificate-github-workflow-ref refs/tags/v<manifest version>
--certificate-github-workflow-sha <manifest source_revision>
```

That is, the manifest was signed by `release.yml` running on the `vX.Y.Z` tag
of this repository that matches its own `version`, at the commit its
`source_revision` names. The updater reads both values from the manifest
before verifying it, and refuses a manifest that does not parse or whose
`source_revision` is not a full commit SHA. The regexp must be anchored (`^`
and `$`); the updater refuses a config whose regexp is not.

Anyone who can push a `v*` tag can get a release signed, so tag creation is
restricted; see [Protect the release tags](#protect-the-release-tags).

**Pinned cosign: v3.1.3.** `release.yml` and `promote.yml` install exactly this
version (`sigstore/cosign-installer` with `cosign-release: v3.1.3`), and hosts
use the same version. Change all three together.

## Host layout

| Path | Owner, mode | What |
| --- | --- | --- |
| `/usr/local/bin/aq-updater` | `root:root 0755` | the updater (`aq-updater-x86_64-linux` from the GitHub Release) |
| `/usr/local/bin/cosign` | `root:root 0755` | cosign v3.1.3 |
| `/etc/agent-q/updater.toml` | `root:aq-updater 0640` | updater config |
| `/etc/agent-q/agent-q.env` | `root:aq-updater 0640` | operator env file: `AQ_VAULT_DIR`, `AQ_SECRETS_DIR` |
| `/etc/agent-q/vault/` | `root:root 0755`, files `0644` | the vault, mounted read-only into the server (UID 10001 must read it) |
| `/etc/agent-q/secrets/` | `root:aq-updater 0750`, files `0644` | `postgres_password`, `server_database_password`, `graph_database_password`, `graph_token`, `tunnel_token` (bind-mounted as-is, so readable by the container users) |
| `/opt/agent-q/compose.yaml` | `root:root 0644` | the production Compose file, from the release tag |
| `/var/lib/agent-q/updater/` | `aq-updater:aq-updater 0750` | updater state: `journal.json`, `history.jsonl`, `release.env`, the lock |
| `/var/lib/agent-q/updater/spool/` | `aq-updater 0755` | the spool shared with the server |
| `/var/lib/agent-q/updater/spool/requests/` | `aq-updater:10001 2770` | server writes request files here (read-write mount) |
| `/var/lib/agent-q/updater/spool/status/` | `aq-updater 0755` | `status.json`, `maintenance` (read-only mount) |
| `/var/lib/agent-q/backups/` | `aq-updater:aq-updater 0700` | pre-update `pg_dump` backups of the server's `aq_server` database (provisioned first with `server-db-init`), the last 5 kept |

`state_dir` may use only the characters `A-Z a-z 0-9 . _ / + @ : -`: its spool
path is written unquoted into `release.env`, and the updater refuses a config
that breaks this rule.

## Install the updater on a host

Everything below runs as root on the host. It assumes Docker Engine with the
Compose v2 plugin (`docker compose version`).

### Users and groups

The updater runs as a dedicated user, `aq-updater`, in the `docker` group.

> **Membership of the `docker` group is equivalent to root.** Anyone who can
> run `docker` can mount the host filesystem into a container. The dedicated
> user keeps the updater's files, logs and registry credentials apart from
> other accounts; it is not a sandbox. Give nobody else access to it.

The server container runs as UID/GID 10001. A host group with GID 10001 lets
the server write requests and the updater read them:

```sh
groupadd --gid 10001 agent-q            # the server container's group
useradd --system --create-home --home-dir /home/aq-updater \
  --shell /usr/sbin/nologin --groups docker,agent-q aq-updater
```

(If GID 10001 is taken on the host, use that group instead; only the number
matters.) The home directory holds the updater's Docker login
(`~/.docker/config.json`) and cosign's Sigstore trust root cache
(`~/.sigstore`).

### Directories and the spool

```sh
install -d -o root -g root -m 0755 /etc/agent-q /etc/agent-q/vault /opt/agent-q
install -d -o root -g aq-updater -m 0750 /etc/agent-q/secrets
install -d -o aq-updater -g aq-updater -m 0750 /var/lib/agent-q/updater
install -d -o aq-updater -g aq-updater -m 0700 /var/lib/agent-q/backups
install -d -o aq-updater -g aq-updater -m 0755 /var/lib/agent-q/updater/spool \
  /var/lib/agent-q/updater/spool/status
# Once, as root: the server (GID 10001) writes here, the updater reads.
install -d -o aq-updater -g 10001 -m 2770 /var/lib/agent-q/updater/spool/requests
```

The updater re-applies the modes on every start (`requests/` 2770, `status/`
0755) but, not being root, leaves the group of `requests/` as set here. The
setgid bit gives every request file group 10001. The server writes request
files with mode 0640, so the updater reads them through its membership of
group 10001.

### cosign

```sh
cd "$(mktemp -d)"
curl -fsSLO https://github.com/sigstore/cosign/releases/download/v3.1.3/cosign-linux-amd64
curl -fsSLO https://github.com/sigstore/cosign/releases/download/v3.1.3/cosign_checksums.txt
grep ' cosign-linux-amd64$' cosign_checksums.txt | sha256sum -c
install -m 0755 cosign-linux-amd64 /usr/local/bin/cosign
cosign version
```

### aq-updater

The binary is attached to each GitHub Release as `aq-updater-x86_64-linux`
(static, built without the `test-verifier` feature), with a `SHA256SUMS`
file. The repository is public, so download both directly:

```sh
base=https://github.com/ElectricJack/aq-server/releases/download/v0.1.0
curl -fsSLO "$base/aq-updater-x86_64-linux" -fsSLO "$base/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
install -m 0755 aq-updater-x86_64-linux /usr/local/bin/aq-updater
aq-updater --version
```

### Compose file, vault, secrets and the operator env file

- `/opt/agent-q/compose.yaml`: `compose.yaml` from the tag you install
  (`git show v0.1.0:compose.yaml`). The updater replaces images, not the
  Compose file: when a release changes `compose.yaml`, its notes say so, and
  you copy the new file before installing that release.
- `/etc/agent-q/vault/config.toml`: a production vault
  (`environment = "production"`, `auth.mode = "cloudflare_access"`) with the
  spool section, which is what turns the update API on:

  ```toml
  [updates]
  spool_dir = "/updater"
  ```

- Start from `vault.production.example/config.toml`. Set the HTTPS public origin,
  Access `team_domain` and application `audience`, and the bootstrap owner email.
  Keep `environment = "production"` and `auth.mode = "cloudflare_access"`.
  Graph uses `AQ_GRAPH_WORKSPACE` (default `e0000000-0000-4000-8000-000000000001`)
  and `AQ_GRAPH_ACTOR` (default `aq-server`); the vault's mutation actor must agree.
- `/etc/agent-q/secrets/` contains independent generated `postgres_password`,
  `server_database_password`, `graph_database_password`, `graph_token`, and the
  Cloudflare `tunnel_token`. Bind-mounted secret files need mode 0644 for the
  container UIDs; keep their host parent directory restricted as above. Never
  put their contents in Compose or the operator env file. The server receives
  only its database password, with the graph token copied into private runtime
  state (UID 10001, mode 0600). The bootstrap PostgreSQL administrator is used
  only by provisioning; `aq_server` and `aq_graph` own separate databases and
  have no superuser/create-role/create-database privileges.
- Retained databases and credentials are checked, not silently reassigned.
  Existing installations that used `agent_q` for server data need an explicit
  backup/migration into the `aq_server` database before adopting this stack;
  Compose does not migrate or delete existing data. Retain the graph token
  across restarts; rotation requires an explicit coordinated change.
- Every service image resolves by digest. Supply `AQ_SERVER_IMAGE` as
  `registry/repository@sha256:<64 lowercase hex digits>` through the updater's
  release env file. Production initialization refuses a mutable reference.
  Server and supervised Git share that image, containing A3b's v0.2.0 binary;
  PostgreSQL, cloudflared and E2's graph v0.1.1 have explicit digests in Compose.
- Copy `scripts/prod-db-init.sh`, `scripts/prod-init-volumes.sh` and
  `scripts/dev-git-supervisor.sh` alongside Compose under its `scripts/`
  directory. The one-shot state initializer has ownership capabilities only
  for project-owned named volumes. Long-running services drop all capabilities,
  use read-only roots and rotated logs. Git state/source staging are shared with
  the server; its healthcheck requires both graph and Git readiness. The server
  has no host port, and PostgreSQL is on the internal database network.
- Run `scripts/compose-smoke.sh prod` for a disposable production-authenticated
  readiness and Git recovery drill. It disables only cloudflared in its overlay.
  Without `AQ_SERVER_IMAGE`, it builds this checkout and obtains a digest using
  a disposable loopback registry; a supplied image must already be digest-pinned.
  The smoke removes its own Compose containers/volumes and registry afterward.
- Add `compose.workers.yaml` to the Compose file list for the cloud worker.
  Build `Dockerfile.worker` using the immutable source and checksum in
  `vendor/worker-candidate.json`, then set `AQ_WORKER_IMAGE` to its published
  manifest digest. [Worker image evidence](implementation/E1c-worker-image.md)
  records the qualified reference and build commands. It includes aq-worker
  0.5.1 with the docker-local controller; the old 0.3.1 image is unsupported.
  Set `AQ_SANDBOX_UID` and `AQ_SANDBOX_GID` to the dedicated rootless sandbox
  daemon's non-root socket owner, and `AQ_SANDBOX_SOCKET_DIR` to its socket
  directory. `aq-worker-state-init` prepares only the journal and credential
  lease named volumes for this identity before the worker starts.
  `AQ_WORKER_CONFIG_DIR` is mounted read-only: place the saved enrollment at
  `agent-q/worker.toml` (0600, readable by the sandbox UID) and the operator
  controller configuration at `controller.json`. Its `state_dir` must be under
  `/state`, and `docker_local.socket` must be `/run/aq-sandbox/podman.sock`.
  The enrollment's `server_url` must be the public HTTPS Access hostname;
  `AQ_SERVER_URL` does not override saved enrollment. The worker joins only
  its own egress network. The root initializer has no network or bind mounts.
  Run `AQ_WORKER_IMAGE=<published-digest> scripts/compose-smoke.sh prod-workers`
  for a disposable worker startup, heartbeat, controller journal and sandbox
  load drill. This test uses a local server route and seeded foundation
  credential; it does not qualify Access enrollment or B2 admission.
- The same overlay includes `aq-worker-boat` (E1d), an independent `boat`
  node. Deploy it first in the desktop-hosted production stack (E5a); use the
  same service on the VPS later (E5b). It has no host sandbox socket or host
  port. Its only network is worker egress, and its node and runner traffic
  must use the public **machine** HTTPS hostname through Cloudflare Access.
  `aq-worker-cloud` continues to use only `docker-local`.

  Enable the Compose profile with `docker compose --profile boat up -d` when
  using `compose.yaml:compose.workers.yaml`. It is opt-in because adding this
  controller to the existing topology needs **4.125 vCPU and 6.625 GiB** of
  service/sandbox ceilings, plus at least **1 decimal GB** for the host
  (minimum 8,113,539,584 bytes total). The unchanged 4-vCPU/8-GB VPS budget
  fails that check. E5b must establish a fitting aggregate budget before
  enabling the same profile there; do not relax the existing smoke guard.

  The default `AQ_BOAT_ENABLED=0`, example config directory and empty key
  sentinel start a `configuration-required` placeholder. Its healthcheck
  reports process availability; that state does **not** mean an enrolled,
  admission-ready worker. Check `docker compose exec aq-worker-boat cat
  /run/aq-boat/status`. No enrollment, provider request or attempt is made
  in this state. Restart the service after completing configuration:

  ```sh
  AQ_BOAT_WORKER_IMAGE=ghcr.io/electricjack/agent-q-worker@sha256:<C7-manifest-digest>
  AQ_BOAT_CONFIG_DIR=/etc/agent-q/worker-boat
  AQ_BOAT_ATTEMPT_KEY_FILE=/etc/agent-q/secrets/boat_attempt_key
  AQ_PUBLIC_HOSTNAME=machine.example.com
  # Set to 1 only after the deployment gates below are met.
  AQ_BOAT_ENABLED=0
  ```

  `AQ_BOAT_WORKER_IMAGE` must contain C7's Boat backend and the existing
  `/usr/local/bin/worker-init-volumes` image helper. The qualified cloud image
  in `deploy/worker-image.env` and the current vendored worker candidate
  predate C7: they can run the placeholder but cannot run Boat attempts.
  Build the separate image from aq-worker C7 commit `c009b6d` (supervisor
  disposition, 2026-11-03), and set the variable to its immutable manifest
  SHA-256, never a tag. C7 release-image publication follows the owner's C7
  PR merge and is outside E1d. A C7-capable image is a deployment prerequisite;
  setting the image variable is not evidence of backend qualification.

  Create the Boat configuration directory with owner 10001:10001, mode
  0700. Enroll a **new** node using a fresh enrollment code and a dedicated
  node Access service token; never copy `AQ_WORKER_CONFIG_DIR`, its
  `agent-q/worker.toml`, node UUID, controller state or credentials. Enrollment
  needs a writable config mount for this one command (the service mounts it
  read-only). With the env variables above exported, run:

  ```sh
  docker run --rm -i --user 10001:10001 \
    --mount "type=bind,src=$AQ_BOAT_CONFIG_DIR,dst=/config" \
    --entrypoint aq-worker "$AQ_BOAT_WORKER_IMAGE" \
    enroll --server "https://$AQ_PUBLIC_HOSTNAME" \
    --name aq-worker-boat --cf-service-token
  ```

  Supply the enrollment code, CF client ID and secret on stdin as prompted,
  never as arguments. Confirm the saved `server_url` is the machine HTTPS
  hostname and both `cf_client_id` and `cf_client_secret` are present. Keep
  `agent-q/worker.toml` mode 0600, readable only by UID 10001. The saved
  enrollment controls transport; `AQ_SERVER_URL` does not override it.
  Provision a second, distinct Access token for this node's sandbox runners
  through the runner material path; never forward the node token or Boat key.

  Copy `deploy/boat.example/controller.json.example` to the directory as
  `controller.json`. Replace all placeholders from the C7 runtime and the
  reviewed template allowlist; set `boat.node_id` to the **new** enrolled
  UUID. C7 refuses a mismatch. `node.templates` must advertise the selected
  qualified templates, and `boat.templates` must contain their pinned image
  digest, saved snapshot name, immutable snapshot ID and scan policy from
  the Boat materialization. The example deliberately advertises none until
  qualification is complete. Configure exactly one `boat` backend, with no
  `docker_local`. Leave `state_dir=/state/controller`,
  `boat.journal_path=/state/boat-mappings.json` and
  `boat.key_file=/run/aq-boat/api-key`. The dedicated `aq_worker_boat_state`
  volume persists controller identity and authoritative sandbox mappings;
  `aq_worker_boat_leases` is also independent of the cloud node. The
  networkless initializer owns only those volumes as UID/GID 10001.

  Use distinct Boat organizations `attempt_org` and `template_org`. The
  attempt key needs sandbox create/read/update/stop/delete, command/file
  operations and read access to the scanned build snapshots. Do not grant
  template snapshot writes, wildcard, account-admin, AI proxy, SSH, desktop
  or hosting rights. Put that key only in `AQ_BOAT_ATTEMPT_KEY_FILE`, owned
  by 10001 with mode 0600. Compose file-backed secrets retain host ownership
  and mode. The entrypoint copies it to node-only tmpfs as a same-owner
  0600 file, as required by C7 `BoatKey`; it is never stored in the state
  volume or passed to sandboxes. The build key has a separate scope/org
  and stays off this service. `boat-build.json.example` shows the C7
  materializer configuration; use it on the separate template build host.

  Keep C7 hard-deadline auto-stop enabled: every VM create/update supplies
  a positive TTL bounded by the original attempt hard deadline plus its
  five-second margin (rounded by less than one second). The controller's
  one-second sweep and five-second deadline margin back up provider TTL.
  Compose's stop grace period is not the VM deadline mechanism. Attempt
  archives remain automatic stopped retention, never paid saved snapshots;
  saved snapshots are for scanned templates only, at most the 10 included.
  Default resource requests are small (2 vCPU / 4 GiB); a request for the
  default size (4 vCPU / 8 GiB) must be explicit. C7 derives size from the
  attempt resource request; it has no `default_size` configuration field.

  Before enabling attempts, establish both owner-mandated budget stops:
  the $20/month Boat plan with auto top-up **off**, run
  `boat billing extra-starts off`, set the dedicated attempt member's usage
  cap within the included $20 of VM time, and concurrent-sandbox cap **4**.
  Keep `node.max_concurrent_sandboxes=4` and
  `node.spend_ceiling={"currency":"USD","micros":18000000}` ($18).
  H2 now prices Boat VM running time in the current UTC calendar month at
  the owner's reference rates: small $0.018/hour, default $0.036/hour,
  large $0.072/hour and xlarge $0.200/hour. Placement refuses at the ceiling
  (`node_spend_ceiling_reached`) or when spend is unknown
  (`node_spend_unaccounted`). **Deployment gate:** use an aq-worker release
  that journals and reports actual VM creation and confirmed stop times to
  `/v1/worker/observations/vm-time`; backfill this node's historical attempts.
  Verify that reporting and the monthly refusal before enabling attempts.
  See [monthly Boat accounting](implementation/H2-monthly-boat-budget.md)
  for the report contract, pinned rates and migration. Do not reset state or
  assume unreported VM use is zero. C7 live provider/template qualification and
  the authenticated admission/runner transport must also be ready.

  Run `scripts/compose-smoke.sh prod-boat` without Boat credentials. It
  starts only this node and its initializer, checks the placeholder,
  isolation, engine limits and state persistence across restart, then
  removes its own containers/volumes. It creates no Boat VMs and does not
  qualify public Access enrollment, monthly accounting or live execution.
- `/etc/agent-q/agent-q.env`, the operator's env file. It holds only the
  operator's paths:

  ```sh
  AQ_VAULT_DIR=/etc/agent-q/vault
  AQ_SECRETS_DIR=/etc/agent-q/secrets
  ```

  Do not put `AQ_SERVER_IMAGE` or `AQ_UPDATER_SPOOL` here; see the next
  section.

### Env files and precedence

The updater runs Compose as:

```
docker compose --project-name agent-q --project-directory /opt/agent-q \
  -f /opt/agent-q/compose.yaml \
  --env-file /etc/agent-q/agent-q.env \
  --env-file /var/lib/agent-q/updater/release.env  ...
```

- The updater owns `<state_dir>/release.env`. It holds `AQ_SERVER_IMAGE` (the
  active release's digest-pinned server image) and
  `AQ_UPDATER_SPOOL=<state_dir>/spool`.
- `release.env` is the last `--env-file`, so its values override the
  operator's env file.
- On a fresh host, with no release installed yet, the updater writes a
  placeholder image (`no-release-installed.invalid/agent-q-server@sha256:000…`)
  so that Compose can parse `compose.yaml` and start postgres.
- **Variables in the process environment override every env file.** Never
  export `AQ_SERVER_IMAGE` or `AQ_UPDATER_SPOOL` for the updater: not in its
  systemd unit, not in your shell before a manual `aq-updater` command.
  `sudo -u aq-updater` resets the environment, which is one more reason to run
  the commands that way.

To run Compose by hand the same way (logs, `ps`, starting postgres), define:

```sh
aqc() {
  sudo -u aq-updater docker compose --project-name agent-q \
    --project-directory /opt/agent-q -f /opt/agent-q/compose.yaml \
    --env-file /etc/agent-q/agent-q.env \
    --env-file /var/lib/agent-q/updater/release.env "$@"
}
```

### updater.toml

```toml
state_dir = "/var/lib/agent-q/updater"
backup_dir = "/var/lib/agent-q/backups"
# Pre-update backups kept in backup_dir (the default; at least 1).
backups_keep = 5
# Defaults, shown for reference:
# release_repo = "ghcr.io/electricjack/agent-q-release"
# channel = "stable"
# readiness_deadline_secs = 120
# poll_interval_secs = 3600
# min_free_bytes = 2147483648
# expected_auth_mode = "cloudflare_access"
# maintenance_drain_secs = 5

[compose]
project_dir = "/opt/agent-q"
files = ["/opt/agent-q/compose.yaml"]
env_files = ["/etc/agent-q/agent-q.env"]
project_name = "agent-q"

[verifier]
kind = "cosign_keyless"
cosign_path = "/usr/local/bin/cosign"
identity_regexp = '^https://github\.com/ElectricJack/aq-server/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$'
oidc_issuer = "https://token.actions.githubusercontent.com"
```

```sh
install -o root -g aq-updater -m 0640 updater.toml /etc/agent-q/updater.toml
```

`maintenance_drain_secs` is how long the updater waits after turning
maintenance on and before it starts the backup, so that an admin write that
passed the maintenance check just before the marker appeared can commit and
land in the backup. `0` turns the wait off.

### Disk space: backups and images

The updater bounds what it keeps on the root filesystem:

- **Backup rotation.** After each pre-update backup it keeps the newest
  `backups_keep` dumps (default 5) in `backup_dir` and deletes older ones. The
  backup of the active release's data (`active_backup`) and the backup of the
  update in flight are never deleted, even when they are older.
- **Free-space checks.** Before pulling, the updater refuses an update when
  `backup_dir`'s filesystem has less free space than the larger of
  `min_free_bytes` and three times the newest backup. After the pull, and
  before maintenance or anything else stops, it checks again and refuses to
  start the backup when free space is below the newest backup's size plus
  10%. Both refusals are recorded as `refused` in the history with the bytes
  free and needed; the active release keeps running. Free some space and
  request the update again.
- **Image prune.** After a successful update (or `restore-previous`), the
  updater removes the local images of the release repositories (the server
  and worker images named in release manifests) other than the current and
  previous release's. Other Compose images (postgres, aq-graph, cloudflared)
  are pinned in `compose.yaml` and not touched. Docker refuses to remove an
  image a container still uses. A failed prune is logged as a warning and
  never changes the update's outcome.

### Registry login (`docker login ghcr.io`)

If the GHCR packages are private (the default for new packages), the updater
needs a login. Public packages need no login: skip this section. Create a
GitHub token that can only read packages:
a classic personal access token with only the `read:packages` scope (or a
fine-grained token limited to reading packages, where GitHub accepts those for
GHCR). Log in **as `aq-updater`**, because Docker reads the credentials of the
user that runs it:

```sh
sudo -u aq-updater -H docker login ghcr.io -u <github-user> --password-stdin <<<'<token>'
```

The credentials land in `/home/aq-updater/.docker/config.json`. If you ever run
the updater as root instead, root's `/root/.docker/config.json` is used and
needs its own login (and files root creates in the state directory then lock
the service out; see [Manual commands](#manual-commands-over-ssh)). Rotate the
token by logging in again; nothing else stores it.

### systemd unit

`/etc/systemd/system/aq-updater.service`:

```ini
[Unit]
Description=Agent Q host updater
Documentation=https://github.com/ElectricJack/aq-server/blob/main/docs/operations.md
Wants=network-online.target docker.service
After=network-online.target docker.service
# At most 5 starts in 10 minutes. Restart= below restarts the updater after a
# halt so recovery runs; if it keeps failing (Docker down, a broken host),
# systemd stops trying instead of looping, and `systemctl status` shows it.
StartLimitIntervalSec=600
StartLimitBurst=5

[Service]
Type=simple
User=aq-updater
Group=aq-updater
ExecStart=/usr/local/bin/aq-updater --config /etc/agent-q/updater.toml run
# SIGTERM goes to aq-updater only. It finishes the step in progress (an
# install included) and starts nothing new; the docker commands it is
# running are not signalled, so they complete.
KillMode=mixed
KillSignal=SIGTERM
# Long enough for an in-flight install: pull, backup, replace, probe, and
# a rollback with a restore. SIGKILL only after that.
TimeoutStopSec=2700
# Exit 3 = halted mid-update (the journal keeps the step); the restart
# runs recovery. Exit 2 = config or usage error, which a restart cannot fix.
Restart=on-failure
RestartSec=10
RestartPreventExitStatus=2
UMask=0027
# No Environment= for AQ_SERVER_IMAGE or AQ_UPDATER_SPOOL: the process
# environment would override release.env.

[Install]
WantedBy=multi-user.target
```

Do not enable it yet: do the first install by hand (next section). After the
start limit is hit, fix the cause and run `systemctl reset-failed aq-updater`
before starting it again.

## First install on a fresh host

1. Initialise the updater's state. This creates the journal, re-applies the
   spool modes and writes `release.env` with the placeholder image and the
   spool path. It installs nothing:

   ```sh
   sudo -u aq-updater aq-updater run --once
   cat /var/lib/agent-q/updater/release.env
   ```

2. Start postgres (the updater backs up and probes through it):

   ```sh
   aqc up -d --wait postgres
   ```

3. Install the release, which pulls and verifies it, starts the server and
   waits for it to report ready:

   ```sh
   sudo -u aq-updater aq-updater install 0.1.0
   sudo -u aq-updater aq-updater status
   ```

4. Start the rest of the stack (cloudflared waits for a healthy server) and
   enable the service:

   ```sh
   aqc up -d
   systemctl daemon-reload
   systemctl enable --now aq-updater
   journalctl -u aq-updater -f
   ```

From now on, updates are requested through the API.

## Requesting an update through the API

Only an owner may request an update; any signed-in user may read the status.
The API sits behind Cloudflare Access, so use `cloudflared access` (it opens a
browser login once and then adds the Access token):

```sh
cloudflared access login https://aq.example.com

# Status: state, active / previous / available releases, recent history.
cloudflared access curl https://aq.example.com/api/updates

# Install a specific version.
cloudflared access curl https://aq.example.com/api/updates \
  -X POST -H 'X-AQ-Request: 1' -H 'Content-Type: application/json' \
  -d '{"version": "0.2.0"}'

# Install the stable channel's current release.
cloudflared access curl https://aq.example.com/api/updates \
  -X POST -H 'X-AQ-Request: 1' -H 'Content-Type: application/json' -d '{}'
```

A request is answered with `202 {"request_id": "…"}`; the updater picks it up
within a few seconds. `X-AQ-Request: 1` is required on every mutation (CSRF
protection). Errors:

| Status | Code | Meaning |
| --- | --- | --- |
| 400 | `invalid_version` | not a plain `X.Y.Z` |
| 403 | | not an owner |
| 409 | `update_in_progress` | an update is running or a request is already queued |
| 503 | `maintenance` | the server is in maintenance during an update (writes are refused) |
| 503 | `updates_unavailable` | no `[updates]` in the vault, or the spool is not readable |

The updater never installs automatically: the hourly channel poll only fills
in `available`. The updater validates every request itself (signature,
upgrade path, worker protocol range, state format, no downgrade) and records
refusals in the history. A release whose worker protocol range shares no
protocol with the active release's is refused: upgrade the workers first, or
install an intermediate release whose range overlaps both.

While the updater fetches and verifies a requested release, `GET
/api/updates` already shows `in_progress` at step `requested` (without a
target for a channel request); a refusal then shows `idle` again.

## Manual commands over SSH

Run the updater's commands as `aq-updater`, so the files they create stay
writable by the service and the right Docker login is used:

```sh
sudo -u aq-updater aq-updater status
```

- `aq-updater status` takes no lock and writes nothing: run it at any time,
  including during an update. It prints the state, the active, previous and
  available releases, and the last 10 history entries. With a torn or
  unreadable `journal.json` it prints the raw file and exits 1 instead of
  guessing.
- `aq-updater install <version>` and `aq-updater restore-previous` take the
  update lock, which the running service holds. **Stop the service first**,
  then start it again:

  ```sh
  systemctl stop aq-updater          # waits for an in-flight update to finish
  sudo -u aq-updater aq-updater restore-previous
  systemctl start aq-updater
  ```

  Without the stop, they exit 1: "The update service holds the lock while it
  runs".

  Once started, `install` and `restore-previous` run to the end. A Ctrl-C,
  a SIGTERM or the SSH session dropping (SIGHUP) does not stop them: the
  first prints `aq-updater: interrupt received; finishing the current
  operation so recovery stays consistent (send SIGKILL to abort; the journal
  will recover on next start)` and the operation continues, with its
  `docker` commands (a `pg_restore`, say) in their own process group. Only
  SIGKILL stops it, and then the next start recovers from the journal. So
  that a dropped connection does not lose the output either, run them in a
  transient unit or in tmux:

  ```sh
  systemd-run --pty --collect --unit aq-updater-manual \
    --uid aq-updater --gid aq-updater \
    /usr/local/bin/aq-updater install 0.2.0
  # or: tmux new -s aq, then sudo -u aq-updater aq-updater ... inside it
  ```

  With `systemd-run` the command runs as its own unit, outside your SSH
  session: if the connection drops it keeps going, and `systemctl status
  aq-updater-manual` and `aq-updater status` show how it went. (`--collect`
  lets the next run reuse the unit name even if this one failed.)
- `aq-updater restore-previous` makes the previous release active again. If
  both releases use the same state format it swaps the images only. If they
  differ, it refuses unless you add `--restore-backup`, which restores the
  backup taken before the active release was installed. **Every change made
  since that backup is lost**, including worker revocations and enrollments:
  a worker revoked since the backup is enrolled again, and one enrolled since
  is unknown. After any restore, check `GET /api/workers`, and revoke or
  re-enroll workers as needed.
- `aq-updater verify-manifest <manifest.json> <manifest.sigstore.json>` checks a
  manifest against the configured verifier (exit 0 valid, 1 invalid, 2 usage
  error).

Exit codes for all commands: 0 success; 1 failure (refused, rolled back,
signature rejected, lock held, corrupt journal); 2 usage or config error; 3
halted mid-update (the next start recovers).

### History detail and logs

History details (in `status`, `status.json` and `GET /api/updates`) are
deliberately short: for a failed command in a container they name the
command, its exit status and the first error line. `status.json` is readable
by every signed-in user, so full output never goes there. The full stderr is
in the journal of the service:

```sh
journalctl -u aq-updater --since '1 hour ago'
```

(For a manual `install` or `restore-previous`, it is on your terminal.)

## `failed_needs_operator`

**What it means:** an update failed, the updater tried once to bring back the
release that was running before, and that failed too. It does not loop:

- the maintenance marker stays on, so the server (if it runs at all) refuses
  writes with `503 maintenance`;
- nothing further is tried automatically, including after a restart or
  reboot (recovery sees the recorded outcome and waits);
- `aq-updater status` shows `state: failed_needs_operator`, the detail, and the
  next steps.

The state can also come from a recovery that cannot tell what happened (for
example, history written by a newer updater after an updater downgrade). The
updater never acts on a guess.

**Steps:**

1. Read the state and the detail: `sudo -u aq-updater aq-updater status`.
2. Find the cause: `journalctl -u aq-updater`, `aqc logs --tail 200 server`,
   `aqc logs --tail 200 postgres`, `aqc ps`, `df -h /var/lib/docker
   /var/lib/agent-q`. Fix what you can (disk space, registry login, a stopped
   Docker daemon).
3. Stop the service so the next command can take the lock:
   `systemctl stop aq-updater`.
4. Retry bringing back the active release. `status` names the command:
   - `sudo -u aq-updater aq-updater restore-previous` when the data was not
     changed by the failed release;
   - `sudo -u aq-updater aq-updater restore-previous --restore-backup` when it
     may have been (the failed release used another state format). This
     restores the pre-update backup named in the detail; changes made after
     it are lost.

   `--restore-backup` restores the recorded pre-update backup whenever one
   exists and the failed release got as far as being started, even when the
   detail does not ask for it. Use it when the logs show the active release
   cannot read the data (for example, the failed release changed it without
   changing its state format). It never restores once the swap to the new
   release was recorded (step `active`), or before the failed release was
   started (the data is still the active release's own).

   **A restore also undoes worker revocations and enrollments made since the
   backup.** After it, check `GET /api/workers`, and revoke or re-enroll
   workers as needed.

   Success prints a `rolled_back` entry ("operator restore: … is running
   again") and clears maintenance. The retry is a single attempt; if it fails,
   the state stays `failed_needs_operator` with the new detail, and you can
   repeat steps 2 to 4.
5. Start the service again: `systemctl start aq-updater`.
6. If the retry cannot succeed (for example the backup is missing), restore by
   hand: the backups are `pg_dump --format=custom` files in
   `/var/lib/agent-q/backups/`, newest first by name. Do not delete
   `journal.json` to get out of this state; ask for help instead.

## Protect the release tags

Do this once, before the first tag. `release.yml` signs whatever a `v*` tag
points at, so only the owner may create those tags. This is the owner's
action in the GitHub UI; nothing in the repository sets it up.

On github.com/ElectricJack/aq-server: **Settings → Rules → Rulesets → New
ruleset → New tag ruleset**, then:

- **Ruleset name:** `release tags`
- **Enforcement status:** Active
- **Bypass list:** add **Repository admin** (the owner), set to *Always
  allow*. Nobody else.
- **Target tags:** *Add target → Include by pattern* `v*`
- **Rules:** check **Restrict creations**, **Restrict updates**, **Restrict
  deletions** and **Block force pushes**.

Create it, then check it is listed as *Active* under **Settings → Rules →
Rulesets**. Tag rulesets have no rule that requires the tagged commit to be on
`main` (at the time of writing). Until GitHub offers one, the owner's
procedure is the safeguard: tag only a merge commit on `main` (step 4 below),
and check before pushing:

```sh
git fetch origin main
git merge-base --is-ancestor <merge-commit> origin/main && echo "on main"
```

## Cutting a release

1. On a branch, bump `[workspace.package] version` in `Cargo.toml` and run
   `cargo update --workspace` so `Cargo.lock` matches (release builds use
   `--locked`).
2. **Every database migration is a state-format change.** A release that
   adds a migration sets `STATE_FORMAT_VERSION` in
   `crates/aq-server/src/lib.rs` to the new migration's number (the migration
   also writes that number to `meta.state_format_version`), and updates
   `READABLE_STATE_FORMATS`. A unit test in `aq-server` fails if they differ.
   The reason: the updater restores the pre-update backup on rollback only
   when the state format changes, while the old server refuses to start on a
   database holding a migration it does not know; a migration without a bump
   would leave the rollback with nothing that can start. The image labels
   (`io.agent-q.state-format`, `io.agent-q.protocol-max`) are derived from the
   source by `release.yml` and checked against the manifest; there is nothing
   to edit in the `Dockerfile`. If the release cannot upgrade from every
   earlier release, narrow `UPGRADE_FROM` too. The updater refuses releases
   that cannot read the installed data; a release that only reads a new
   format needs a tested migration and restore procedure first.
3. Open a PR, wait for `ci` to pass, and merge it.
4. Tag the merge commit on `main` and push the tag:

   ```sh
   git tag v0.2.0 <merge-commit>
   git push origin v0.2.0
   ```

5. `release.yml` runs on the tag (`gh run watch`). In order, it:
   - checks the tag equals the workspace version;
   - builds and pushes the server and worker images, stamped with the tagged
     commit;
   - generates `manifest.json` with the new server image's own
     `aq-server manifest`;
   - checks the pushed images' `io.agent-q.state-format` and
     `io.agent-q.protocol-max` labels equal the manifest's `state_format` and
     `protocol.max`;
   - signs it keyless with `cosign sign-blob` (the workflow's OIDC identity);
   - builds the static `aq-updater` and verifies the manifest with it, using the
     production identity;
   - runs `scripts/smoke` against the pushed digests;
   - only then pushes `agent-q-release:<version>`;
   - creates the GitHub Release with `manifest.json`,
     `manifest.sigstore.json`, `aq-updater-x86_64-linux` and `SHA256SUMS`.

   The release image is pushed only after the smoke test passes, and the
   updater and `promote.yml` find releases only through it. A run that fails
   before then leaves at most server and worker images, which no host can
   install. If it fails, fix the cause and cut a new patch version. Do not
   move the tag.
6. Promote it to the stable channel:

   ```sh
   gh workflow run promote.yml -R ElectricJack/aq-server -f version=0.2.0
   gh run watch -R ElectricJack/aq-server
   ```

   `promote.yml` requires the GitHub Release to exist (that is, the release
   run finished, smoke test included). It resolves
   `agent-q-release:<version>` to its digest, checks the manifest in that
   image names the version, verifies the manifest and bundle with cosign, the
   production identity and the same tag and commit bindings as the updater,
   and only then points `agent-q-release:stable` at that digest (`docker
   buildx imagetools create --prefer-index=false`).
7. Hosts show the new release as `available` after their next channel poll
   (within an hour). Request the update through the API.

## Upgrading the updater by hand

Releases do not replace `aq-updater` itself; the spec defers that. To upgrade
it:

```sh
gh release download v0.2.0 -R ElectricJack/aq-server -p aq-updater-x86_64-linux -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
sudo -u aq-updater aq-updater status        # expect state: idle
systemctl stop aq-updater                   # waits for an in-flight update
install -m 0755 aq-updater-x86_64-linux /usr/local/bin/aq-updater
aq-updater --version
systemctl start aq-updater
sudo -u aq-updater aq-updater status
```

Upgrade only while idle. A newer updater reads the older journal and history.
Avoid downgrading the updater: history it does not understand makes recovery
stop at `failed_needs_operator`. Also re-read the release notes for changes to
`updater.toml` or the unit.

## Installing a worker release by hand

Workers run on other machines and are not updated by `aq-updater`. Install
them from the worker image digest in a verified manifest:

1. Get the manifest and bundle from the GitHub Release and verify them:

   ```sh
   gh release download v0.2.0 -R ElectricJack/aq-server -p manifest.json -p manifest.sigstore.json
   SHA=$(python3 -c 'import json; print(json.load(open("manifest.json"))["source_revision"])')
   cosign verify-blob --bundle manifest.sigstore.json \
     --certificate-identity-regexp '^https://github\.com/ElectricJack/aq-server/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$' \
     --certificate-oidc-issuer https://token.actions.githubusercontent.com \
     --certificate-github-workflow-ref refs/tags/v0.2.0 \
     --certificate-github-workflow-sha "$SHA" \
     manifest.json
   WORKER=$(python3 -c 'import json; print(json.load(open("manifest.json"))["images"]["worker"])')
   echo "$WORKER"     # ghcr.io/electricjack/agent-q-worker@sha256:…
   ```

2. On the worker machine, log in to GHCR with a `read:packages` token as
   above if the packages are private, then pull the digest:

   ```sh
   docker pull "$WORKER"
   ```

3. First time only, enroll. Create a code with
   `POST /api/enrollments` (owner), then:

   ```sh
   docker run --rm -i -v aq-worker-config:/config "$WORKER" \
     enroll --server https://<worker-endpoint> --name "$(hostname)"
   ```

   The worker's identity is kept in the `aq-worker-config` volume.

4. Run it, or replace the running one, by digest:

   ```sh
   docker rm -f aq-worker 2>/dev/null || true
   docker run -d --name aq-worker --restart unless-stopped \
     --read-only --cap-drop ALL --security-opt no-new-privileges:true \
     -v aq-worker-config:/config "$WORKER"
   ```

   `GET /api/workers` shows the node online again. The worker and the server
   must share a protocol version; the manifest's `protocol` range says which.

## Not covered here

These belong to the hosted-access plan; see it for the procedures:

- provisioning the VPS, Cloudflare Tunnel and Cloudflare Access, and the
  worker endpoint;
- signing out (logout) and the rest of the phase 1 access items;
- the live demonstrations on the real host: a successful self-update, a
  failed one, and an interrupted one with a reboot.

Off-host encrypted backups and restore drills belong to the recovery proof
plan.
