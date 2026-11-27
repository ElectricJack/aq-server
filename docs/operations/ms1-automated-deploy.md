# aq-ms1: deploys by merge to main

After the one-time cutover below, a merge to `main` of aq-server or aq-worker
reaches the live single-host stack without anyone running a command:

| Repository | On merge to main (GitHub Actions) | On the host (systemd user timer) |
| --- | --- | --- |
| aq-server | `server-image.yml` pushes `ghcr.io/aq-platform/aq-server:sha-<sha>` and moves `:main` | `aq-ms1-deploy.timer` runs `ms1-deploy.py apply` |
| aq-worker | `worker-image.yml` pushes `ghcr.io/aq-platform/aq-worker:sha-<sha>` and moves `:main` | `aq-ms1-worker-deploy.timer` runs `ms1-worker-deploy.py apply` |

`:main` moves only while the built commit is still main's head, so a late,
older build never replaces a newer one. Both host scripts do nothing when the
stack already runs the `:main` digest, and never retry a digest that already
failed on this host. Their docstrings (`scripts/ms1-deploy.py` here,
`scripts/ms1-worker-deploy.py` in aq-worker) describe every step and exit
status.

## Why not `aq-updater`

The format-1 host updater (`crates/aq-updater`, [operations.md](../operations.md))
was built for the production layout. aq-ms1 runs the development overlay, and
the updater cannot drive it without a different stack:

| The updater needs | aq-ms1 has |
| --- | --- |
| A cosign-signed `vX.Y.Z` release from `release.yml`, newer than the active one | One version, `0.1.0`, for every main commit: refused as `SameVersion` |
| An owner request through `POST /api/updates` (it never installs on its own) | No request path: the goal is merge to main |
| The `[updates]` spool in a production vault, mounted into `server` | The dev vault volume; `compose.dev.yaml` overrides the server volumes |
| Server state in `aq_server`, provisioned by `server-db-init` | Server and playbook state in `agent_q` |
| `expected_auth_mode = "cloudflare_access"` by default | `auth_mode: dev` |
| Release images built without features | `project_environments`, `supervisor_full_access` and `delivery_playbook` |

`ms1-deploy.py` keeps the parts of the updater's contract that matter here:
digest-pinned images, a pre-update `pg_dump`, a readiness probe, one automatic
rollback that restores the backup when a migration already ran, a recorded
in-flight step that the next run finishes or rolls back, and a hard stop
(`needs_operator`) when the rollback fails.

## What a deploy does

**Server** (`ms1-deploy.py apply`):

1. Pull `[deploy] image`, pin its registry digest, read its revision and
   state-format labels. Refuse a state-format downgrade, and refuse when the
   rendered Compose config would change anything but image fields.
2. Turn the owner's availability off (only if it is on): no new launches.
   Running agents keep running on their workers.
3. Wait up to `quiet_timeout_secs` until no supervisor is mid-turn. Otherwise
   restore availability and exit 75 (the next tick tries again).
4. `pg_dump -Fc --create agent_q` into `<state_dir>/backups`, checked with
   `pg_restore --list`; the newest `backups_keep` are kept.
5. Write `AQ_SERVER_IMAGE` into `<state_dir>/release.env` and recreate only the
   server (`up -d --no-deps --no-build --pull never --force-recreate server`).
6. Wait up to `ready_timeout_secs` for `aq-server readiness` to report the
   image's revision and state format with `database: ok`, and for
   `dependency-readiness` to report `ready`. Migrations run in this window.
7. Wait up to `reconnect_timeout_secs` for every worker node that was online to
   report again. One sleeping machine only logs a warning; none at all is a
   failure.
8. Commit, restore availability, prune backups and the repository's old local
   images (the current and previous release are kept).

Any failure in 5 to 7 rolls back once: stop the server, restore the backup if
the database's state format moved, start the previous image, wait for its
readiness, restore availability. If that fails, availability stays off,
`state.json` holds `needs_operator`, and every later run refuses until `ack`.

**Workers** (`ms1-worker-deploy.py apply`), one machine at a time, for every
machine with a hosted receiver seen in the last `receiver_fresh_secs`:

1. Publish once per revision: copy `/usr/local/bin/aq-worker` out of the image,
   require `--version` to end in `+<revision>`, and run that revision's
   `prepare-worker-install.py --base-bundle` on the gateway's current bundle.
   It keeps the bundle's host-pinned contents byte for byte and replaces the
   worker and the installer files. Bundle and installer land in
   `<gateway>/install/<sha256>.tar.gz|.sh`.
2. `aq worker drain` the machine if it was taking work (an owner's drain or
   stop is left alone), and wait up to `drain_timeout_secs` until no agent and
   no hand-started session runs on it. Otherwise restore admission and exit 75.
3. `POST /api/worker-diagnostics/<node>` with `receiver_update_v1` and the new
   installer. Require the receipt to name the new worker, then every node of
   the machine to be online with the revision and a capability inventory
   received after the request.
4. `aq worker start`. A failed step 3 is rolled back with the previous
   installer; if that fails, the machine stays drained and is held until
   `ms1-worker-deploy.py release <machine>`.
5. The server's install manifest (`manifest/install-manifest.json`, which the
   dashboard's "Add or update machine" command reads) moves to the new
   installer only once a machine runs it.

A receiver update replaces only `~/.local/share/agent-q/runtime/aq-worker` and
the receiver. The bundle's other programs (session host, `aq-agent`,
`aq-git`, tmux, Podman, Codex helpers) and each profile's `native.json` change
only with a full install, which stays an operator action.

## Owner prerequisites

- **GHCR packages.** The first push of each workflow creates
  `ghcr.io/aq-platform/aq-server` and `ghcr.io/aq-platform/aq-worker`,
  private and linked to their repository. If Settings → Actions → General
  restricts workflow permissions, allow the jobs' `packages: write`.
- **A read-only registry login for the host.** Docker's `desktop.exe`
  credential store fails outside an interactive session (a `docker buildx
  build --check` from an agent shell already fails with `error getting
  credentials`), so the timers use their own `DOCKER_CONFIG`. Create a classic
  personal access token with only `read:packages`; the owner runs step A3 with it.
- **Merge** the aq-server and aq-worker pull requests that add this.

## One-time cutover

Run each step, then its check. If a check fails, run that step's rollback and
stop. Until phase A ends, nothing here changes how the live stack runs.

Shell variables used throughout:

```sh
AQ=$HOME/dev/aq-platform
LIVE=$AQ/wt/authoring-deploy-server             # the live Compose project; never delete
MS1=$HOME/.local/share/aq-ms1
OVERLAY=$MS1/intelligence-settings.compose.yaml
DEPLOY=$MS1/deploy                              # ms1-deploy.py state_dir
O="--local --origin http://127.0.0.1:28080"
```

### Phase A: server

**A1. Preflight.**
```sh
docker inspect aq-ms1-server-1 --format '{{.Image}} {{index .Config.Labels "org.opencontainers.image.revision"}}'
docker exec aq-ms1-server-1 aq-server readiness
grep -c 'image: sha256:' "$OVERLAY"
```
Check: the server image and revision are the ones the overlay pins on its
`server` service, readiness says `database: ok`. Rollback: none (read-only).

**A2. The first `:main` images exist.**
```sh
gh run list -R aq-platform/aq-server -w server-image.yml -L 3
gh run list -R aq-platform/aq-worker -w worker-image.yml -L 3
```
Check: the run for the merge commit is green and its summary names the
digest. Rollback: none; fix the workflow and re-run it (`gh workflow run`).

**A3. Registry login for the timers** (the owner types the token).
```sh
install -d -m 0700 ~/.config/agent-q/docker
DOCKER_CONFIG=~/.config/agent-q/docker docker login ghcr.io -u ElectricJack --password-stdin
DOCKER_CONFIG=~/.config/agent-q/docker docker pull --quiet ghcr.io/aq-platform/aq-server:main
DOCKER_CONFIG=~/.config/agent-q/docker docker image inspect ghcr.io/aq-platform/aq-server:main \
  --format '{{index .Config.Labels "org.opencontainers.image.revision"}} {{index .Config.Labels "io.agent-q.state-format"}}'
```
Check: the revision is the merge commit and the state format is at least the
live one. Rollback: `rm -r ~/.config/agent-q/docker`.

**A4. Install the script from the merged commit.**
```sh
git -C $AQ/aq-server fetch origin
rev=$(git -C $AQ/aq-server rev-parse origin/main)
install -d -m 0700 $DEPLOY $DEPLOY/bin
git -C $AQ/aq-server show "$rev:scripts/ms1-deploy.py" > $DEPLOY/bin/ms1-deploy.py
echo "$rev" > $DEPLOY/bin/REVISION
```
Check: `python3 -I $DEPLOY/bin/ms1-deploy.py --help` prints the usage.
Rollback: `rm -r $DEPLOY`. To update the script later, repeat this step; the
timers never update it themselves.

**A5. The Compose environment as an env file.** Rollouts so far passed it as
JSON in the process environment; `AQ_SERVER_IMAGE` now comes from
`release.env` only, so it must not be in this file or in the environment.
```sh
python3 - "$MS1/role-catalog-rollout-20261117/compose-environment-after.json" "$DEPLOY/compose.env" <<'PY'
import json, sys
env = json.load(open(sys.argv[1]))
for key in ("AQ_SERVER_IMAGE", "COMPOSE_PROJECT_NAME"):
    env.pop(key, None)
open(sys.argv[2], "w").write("".join(f"{k}={v}\n" for k, v in sorted(env.items())))
PY
chmod 600 $DEPLOY/compose.env
printf 'AQ_SERVER_IMAGE=%s\n' "$(docker inspect aq-ms1-server-1 --format '{{.Image}}')" > $DEPLOY/release.env
```
Check: `grep -c AQ_SERVER_IMAGE $DEPLOY/compose.env` prints 0 and
`release.env` names the running image ID. Rollback: delete both files.

**A6. Let the overlay take the server image from `release.env`.** This is the
only edit to the live Compose files.
```sh
cp -p "$OVERLAY" "$OVERLAY.before-ms1-deploy"
running=$(docker inspect aq-ms1-server-1 --format '{{.Image}}')
test "$(grep -c "^    image: $running\$" "$OVERLAY")" = 1
sed -i "s#^    image: $running\$#    image: \${AQ_SERVER_IMAGE:?set by ms1-deploy release.env}#" "$OVERLAY"
aqc() {
  env -u AQ_SERVER_IMAGE docker compose --project-name aq-ms1 --project-directory "$LIVE" \
    -f "$LIVE/compose.yaml" -f "$LIVE/compose.dev.yaml" -f "$LIVE/compose.dashboard.yaml" -f "$OVERLAY" \
    --env-file "$DEPLOY/compose.env" --env-file "$DEPLOY/release.env" "$@"
}
aqc config --hash server
docker inspect aq-ms1-server-1 --format '{{index .Config.Labels "com.docker.compose.config-hash"}}'
```
Check: the two hashes are equal, so the running container already matches and
nothing is recreated (verified read-only on 2026-11-26 against a copy of the
overlay: both `25318290…c378`). `diff "$OVERLAY.before-ms1-deploy" "$OVERLAY"`
shows exactly the one `image:` line. Rollback:
`cp -p "$OVERLAY.before-ms1-deploy" "$OVERLAY"`.

**A7. Configuration.**
```sh
cat > ~/.config/agent-q/ms1-deploy.toml <<EOF
[deploy]
state_dir = "$DEPLOY"
image = "ghcr.io/aq-platform/aq-server:main"
origin = "http://127.0.0.1:28080"
aq = "$HOME/.local/bin/aq"
quiet_timeout_secs = 900        # supervisors mid-turn: defer after this
ready_timeout_secs = 600        # startup plus migrations
reconnect_timeout_secs = 360    # workers report again
poll_secs = 5
backups_keep = 5
command_timeout_secs = 120
backup_timeout_secs = 1800      # pg_dump, pg_restore, docker pull

[compose]
project_name = "aq-ms1"
project_dir = "$LIVE"
files = ["$LIVE/compose.yaml", "$LIVE/compose.dev.yaml", "$LIVE/compose.dashboard.yaml", "$OVERLAY"]
env_files = ["$DEPLOY/compose.env"]
EOF
DOCKER_CONFIG=~/.config/agent-q/docker python3 -I $DEPLOY/bin/ms1-deploy.py status
```
Check: `status` adopts the running server: `current.revision` is the live
revision, `running_image` its ID, `in_flight` and `needs_operator` null. It
writes nothing until the first `apply`. Rollback: delete the config.

**A8. First deploy, watched.** Run it where a dropped terminal does not stop
it:
```sh
systemd-run --user --pty --collect --unit aq-ms1-deploy-first \
  -E DOCKER_CONFIG=$HOME/.config/agent-q/docker \
  /usr/bin/python3 -I $DEPLOY/bin/ms1-deploy.py apply
```
Check: it ends with `deployed <revision>`; `status` shows the new `current`
and the old release as `previous`; `aq $O --json swarm` (or the dashboard)
shows availability back on, the same running agents and every worker online.
Rollback: it rolls back by itself on a failed check. To undo a deploy that
passed its checks but misbehaves:
`python3 -I $DEPLOY/bin/ms1-deploy.py rollback --restore-backup` (the
`--restore-backup` is needed when the state format changed; every change since
that backup is lost, so check `aq $O worker list` afterwards).

**A9. The timer.**
```sh
cat > ~/.config/systemd/user/aq-ms1-deploy.service <<'EOF'
[Unit]
Description=AQ ms1: deploy the server image of aq-server main
After=docker.service

[Service]
Type=oneshot
Environment=DOCKER_CONFIG=%h/.config/agent-q/docker
ExecStart=/usr/bin/python3 -I %h/.local/share/aq-ms1/deploy/bin/ms1-deploy.py apply
# 75: deferred (busy or not quiet); the next tick retries.
SuccessExitStatus=75
# The script bounds every wait itself (ms1-deploy.toml).
TimeoutStartSec=infinity
KillMode=mixed
EOF
cat > ~/.config/systemd/user/aq-ms1-deploy.timer <<'EOF'
[Unit]
Description=AQ ms1: check aq-server main for a new server image

[Timer]
OnBootSec=5min
OnUnitInactiveSec=5min

[Install]
WantedBy=timers.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now aq-ms1-deploy.timer
```
Check: `systemctl --user list-timers aq-ms1-deploy.timer` shows the next run;
after it, `journalctl --user -u aq-ms1-deploy -n 20` says `server already runs
<revision>`. Rollback: `systemctl --user disable --now aq-ms1-deploy.timer`.

From here a merged aq-server change reaches the server within the build time
plus five minutes. To stop deploys, disable the timer.

### Phase B: worker publishing and the laptop

The laptop (`desktop-7lvb545`, node `54cc300b-…`) is the only machine with a
hosted receiver today; the desktop profiles are skipped until phase C.

**B1. Configuration and state.**
```sh
cat > ~/.config/agent-q/ms1-worker-deploy.toml <<EOF
origin = "http://127.0.0.1:28080"
aq = "$HOME/.local/bin/aq"
image = "ghcr.io/aq-platform/aq-worker:main"
gateway = "$MS1/worker-install-gateway"
manifest = "$MS1/worker-install-gateway/manifest/install-manifest.json"
source = "$AQ/aq-worker"                # a clone; git archive of each revision's packaging
state_dir = "$MS1/worker-deploy"
# The 384 MiB the desktop profiles need (Home and Dashboard sources) for every
# machine that installs the bundle. Receiver updates never change a profile's native.json.
max_workspace_bytes = 402653184
drain_timeout_secs = 1800
update_timeout_secs = 900
receiver_fresh_secs = 120
poll_secs = 10
bundles_keep = 5
command_timeout_secs = 600
EOF
git -C $AQ/aq-worker show origin/main:scripts/ms1-worker-deploy.py > $DEPLOY/bin/ms1-worker-deploy.py
python3 -I $DEPLOY/bin/ms1-worker-deploy.py status
```
Check: `machines` lists every machine with its worker versions and `manifest`
is the gateway's current one (`74078ef7…`, worker `d51435ca` on 2026-11-26).
Rollback: delete the config and `$MS1/worker-deploy`.

**B2. First worker deploy, watched** (the laptop must be idle or it is
deferred):
```sh
systemd-run --user --pty --collect --unit aq-ms1-worker-deploy-first \
  -E DOCKER_CONFIG=$HOME/.config/agent-q/docker \
  /usr/bin/python3 -I $DEPLOY/bin/ms1-worker-deploy.py apply
```
Check: `published installer …`, `desktop-7lvb545: runs <revision> with a
fresh inventory`, `admission restored`, `install manifest now names …`; in
`aq $O --json worker list` the laptop's node reports `0.5.1+<revision>`.
Rollback: automatic on a failed check. By hand: take
`published[-1].previous.installer_sha256` from `state.json`, send it the same
way (`curl -X POST -H 'X-AQ-Request: 1' -H 'Content-Type: application/json'
-d '{"operation_id":"<new uuid>","kind":"receiver_update_v1","installer_sha256":"<sha>"}'
http://127.0.0.1:28080/api/worker-diagnostics/<node>`) and write `previous`
back into the manifest file.

**B3. The timer.** As A9, with `aq-ms1-worker-deploy.service` running
`ms1-worker-deploy.py apply` and the same `DOCKER_CONFIG`,
`SuccessExitStatus=75` and `TimeoutStartSec=infinity`, and a timer with
`OnUnitInactiveSec=10min`. Check and rollback as A9.

**B4. Remove the stale manifest.** `$MS1/worker-install-gateway/install-manifest.json`
(2026-11-15) is read by nothing: nginx serves only `install/<sha>`, and the
server mounts `manifest/`. Check `grep -r install-manifest $MS1/worker-install-gateway/nginx.conf`
finds nothing, then `mv` it to `install-manifest.stale-20261115.json`.

### Phase C: move the desktop profiles to hosted installs

Both desktop profiles are operator-managed: `aq-node@ms1` and the transient
`aq-node-desktop-pg-canary` run binaries from rollout directories through unit
drop-ins, with hand-kept `native.json` files, and enrolled through the
loopback worker endpoint. A hosted receiver cannot adopt them in place: the
receiver requires its slots' `server_url` to be the HTTPS install origin, and
the installer refuses a profile enrolled elsewhere ("already configured for a
different installation"). So each becomes a new hosted profile with a new
enrollment, and the old one is revoked after the new one is verified. Node IDs
change; the number of nodes and slots does not.

Owner decisions before C2:

1. New machine keys (`aq worker revoke` acts on a whole machine, so the new
   nodes must not share the old machine's key). Suggested: `desktop-hosted`
   and `desktop-pg-canary-hosted`, with the old labels.
2. Whether `max_workspace_bytes = 402653184` applies to every hosted machine
   (phase B's config does that for new installs) or only to the desktop.
3. Whether the `postgres17-canary@2` environment approval and the template
   registry entry are bound to the canary's node ID; if so, approve them again
   for the new node before C4.

**C1. Clear the stale shared runtime.** The hosted installer shares
`~/.local/share/agent-q/runtime` across profiles and refuses a different one.
The one there dates from 2026-11-08 and nothing runs it (the units' drop-ins
start other binaries).
```sh
for p in $(pgrep -x aq-worker); do readlink /proc/$p/exe; done | sort | uniq -c
```
Check: no process runs `~/.local/share/agent-q/runtime/aq-worker`. Then
`mv ~/.local/share/agent-q/runtime ~/.local/share/agent-q/runtime.before-hosted`
and move `~/.local/bin/aq-worker` (a link into it) aside the same way.
Rollback: move both back.

**C2. The ordinary desktop.**
1. `aq $O worker drain --machine desktop --expected-generation <control.generation>`;
   wait until `aq $O --json swarm` shows no agents on Local Worker 1 and 2 and
   `aq $O --json session manual list` no running session on them.
2. `systemctl --user stop aq-node@ms1` (keep it enabled until step 5).
3. `aq $O worker enroll` for a code, then run the published installer as the
   dashboard's "Add or update machine" shows it, with `--profile desktop
   --nodes 2 --machine-key desktop-hosted --name "Jack's Windows Desktop"`, and
   `AQ_CLAUDE_TOKEN_FILE` set to `claude_token_file` from
   `~/.config/agent-q/service/ms1/service.toml` so hand-started Claude sessions
   keep working.
4. Check: the installer ends with "Worker ready"; `aq-node@desktop` runs
   `~/.local/share/agent-q/runtime/aq-worker`; `/api/worker-diagnostics` lists
   receivers for both new nodes; the new `native.json` has
   `max_workspace_bytes` 402653184; the new `service.toml` lists the same 13
   repositories as `ms1/service.toml` and has a `[manual]` section (compare
   `max_slots`, 8 on the old profile, and set it if the installer left it out).
   Set the new machine's agent maximum to the old one's (8).
5. `systemctl --user disable aq-node@ms1`, then
   `aq $O worker revoke --machine desktop`. Keep the old profile directory and
   drop-ins until the new machine has run real work for a day, then delete the
   drop-ins under `~/.config/systemd/user/aq-node@ms1.service.d/`.

Rollback (before 5): `systemctl --user disable --now aq-node@desktop`,
`aq $O worker revoke --machine desktop-hosted`, `systemctl --user start
aq-node@ms1`, `aq $O worker start --machine desktop --expected-generation <g>`.

**C3. Make the canary's sandbox daemon persistent.** Today it is the transient
unit `aq-sandbox-pg-canary.service`, with a random runtime directory
(`/run/user/1000/aq-pg-canary.iACRE6`), so it does not survive a reboot.
Write a real unit with the same `ExecStart`, `Delegate=cpu memory pids`,
`KillMode=process` and environment, and a fixed runtime directory
(`XDG_RUNTIME_DIR=%t/aq-pg-canary`, `AQ_SANDBOX_PREFIX` and
`AQ_SANDBOX_SINGLE_UID=0` as now). Move the daemon's script and prefix
(`pg-daemon/`) out of the rollout directory into `$MS1/pg-canary-daemon/`
first. Check: after `systemctl --user restart aq-sandbox-pg-canary`, the
socket under the fixed directory answers `podman --remote … info` with
`rootless` and cgroup v2. Rollback: stop the new unit and start the
transient one again with the command in `systemctl --user cat
aq-sandbox-pg-canary`.

**C4. The PostgreSQL canary.** The generic bundle carries one sandbox
template; the canary needs `postgres17-canary@2`. Install a hosted profile,
then give it the canary template. Receiver updates replace only the worker
binary and the receiver, so the template survives every automatic update.
1. Drain `desktop-pg-canary`, wait until idle, stop
   `aq-node-desktop-pg-canary`.
2. Install with `--profile pg-canary --nodes 1 --machine-key
   desktop-pg-canary-hosted --name "Desktop PostgreSQL canary"`.
3. Stop `aq-node@pg-canary`. Build its `native.json` from
   `~/.config/agent-q/service/desktop-pg-canary/postgres17-canary-v2/native.json`
   with the hosted profile's paths, as the installer's `prepared_template`
   writes them: `host.root`, `helper.path` and `tmux.path` into the installed
   package, `state_dir`, `git_root`, `git_socket` and `node_git.repositories`
   under `~/.config/agent-q/service/pg-canary`, and
   `docker_local.socket` = the fixed canary socket from C3. Copy the two
   `pg17-*-adapter.json` files next to it (mode 0600) and point the template at
   them.
4. Add the drop-in `~/.config/systemd/user/aq-node@pg-canary.service.d/10-pg-daemon.conf`:
   `[Unit]` `Requires=aq-sandbox-pg-canary.service` and
   `After=aq-sandbox-pg-canary.service`. This is configuration, not a
   binary path: receiver updates keep working.
5. Start `aq-node@pg-canary`. Check: AQ shows the new node online advertising
   `postgres17-canary@2` with both harnesses, its receiver is listed, and a
   fresh inventory is accepted. Then revoke `desktop-pg-canary`.
6. Never re-run the generic installer on this profile: a full install would
   replace its `native.json` with the bundle's. Repeat step 3 after one.

Rollback (before revoking the old machine): stop and disable
`aq-node@pg-canary`, revoke `desktop-pg-canary-hosted`, start
`aq-node-desktop-pg-canary` with its existing command, `aq worker start` the
old machine.

**C5. Check the one flow.** `ms1-worker-deploy.py status` lists three
machines with receivers, and the next `:main` worker reaches all three
through `apply`.

## Day to day

- Status: `python3 -I ~/.local/share/aq-ms1/deploy/bin/ms1-deploy.py status`
  and `… ms1-worker-deploy.py status`; logs in
  `journalctl --user -u aq-ms1-deploy -u aq-ms1-worker-deploy`.
- Retry a digest that failed: `apply --retry-failed`.
- Deploy a specific build: `apply --image ghcr.io/aq-platform/aq-server@sha256:…`.
- After `needs_operator`: fix the stack (it runs whatever the rollback left;
  `release.env` names the previous image), then `ms1-deploy.py ack`.
- After a held worker machine: fix it, `aq worker start` it, then
  `ms1-worker-deploy.py release <machine>`.
- A release that changes Compose files (`compose*.yaml`, `scripts/*` the
  services mount) is refused by the config check: bring `wt/authoring-deploy-server`
  to that commit by hand first. The timer only moves images.

## Risks

- **No migration rehearsal.** The hand rollouts rehearsed each migration on a
  restored copy before going live. `ms1-deploy.py` relies on the backup and
  rollback instead; a migration that corrupts data but still reports ready is
  not caught.
- **Restore loses writes.** A rollback that restores the backup loses what
  workers and supervisors wrote after it (minutes, with availability off).
- **Hand-started sessions and agent grants during the server restart.** The
  server is down for its startup and migrations. Running agents keep running
  on their workers, and earlier rollouts kept every attempt across a restart,
  but nothing here re-checks grant expiry the way those rollouts did.
- **Workers wait for idle.** A machine with long-running work defers its update
  until a drain finds it idle within `drain_timeout_secs`; meanwhile it stays
  on the previous worker.
- **Scripts do not update themselves.** Steps A4 and B1 install them from a
  commit; a change to either script needs that step again. The worker's
  packaging code, in contrast, comes from each revision.
- **Compose files are not deployed** (see Day to day).
