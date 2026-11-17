# Getting started: a complete local AQ stack

This is the single entry point for running AQ v2 on your own machine from
source: the server, PostgreSQL, aq-git, aq-graph, the current dashboard, the
internal work host (routing) and a local worker that runs real tasks on Claude
models in sandboxes. One tool does every step: [`scripts/local-stack`](../scripts/local-stack).

## Status of this guide (2026-11-17, work in progress)

The sequence is being proven from fresh clones on Ubuntu 24.04 under WSL2,
as a second stack beside a running one. This block is updated as the run
progresses; treat every step not marked "passed" as untested.

| Step | State |
| --- | --- |
| `doctor` | Passed. |
| `build` (all seven parts) | Passed from fresh clones: about 24 minutes for the images, dashboard and worker programs with 8 cargo jobs, plus about 25 minutes for the sandbox template (after a fix to `scripts/package-sandbox.py` in aq-worker). |
| `up`, `seed`, `pins`, `internal-work`, `worker` | Passed, each after fixes that are now in the scripts. They ran one at a time while being fixed. |
| `task` | Passed: four tasks ran Claude in sandboxes on the local worker, ended `success` and published commits to the scratch remote; two ran at the same time on the node's two slots. |
| `routing` | **In progress.** The authoring run works (Claude Opus on the internal work host); its first run stopped with a question because the routing document named agents this install does not have. The shipped document was rewritten and is being authored now. |
| One uninterrupted `all` from an empty state | **Not run yet.** |
| `reload …`, `down` | `down -v` passed. `reload` not run yet. |

What is known not to be covered is listed under [Not done](#not-done).

## Prerequisites

| Need | Version | Notes |
| --- | --- | --- |
| Linux x86-64, or WSL2 with systemd | Ubuntu 24.04 was used | WSL2: `[boot] systemd=true` in `/etc/wsl.conf`, then `wsl --shutdown` once. Keep the checkouts in the Linux filesystem. |
| Docker with Compose | Compose 2.24.4 or newer | Docker Desktop's WSL integration works. |
| Rust | 1.96.1 | Installed by `rustup` from each repository's `rust-toolchain.toml`. |
| Python | 3.12 or newer | |
| Node and pnpm | Node 24, pnpm 10.34.5 | For the dashboard (`corepack enable` gives the pinned pnpm). |
| `git`, `curl`, `tmux` | tmux at `/usr/bin/tmux` | The worker pins the tmux it starts agents in. |
| `uidmap` package | optional | Without it the sandbox daemon runs every container as one host UID. |
| About 40 GB of free disk | | Cargo targets, images and one 2 GB sandbox template. |
| A Claude subscription | | See "The provider login". |

The worker's sandboxes run in a dedicated rootless Podman daemon. You do not
install Podman: `local-stack` downloads a pinned static build (checked by
SHA-256) into `~/.local/opt/aq-sandbox-podman` and runs it as the user service
`aq-sandbox-podman.service`.

## Clone layout

Clone the six repositories side by side. The tool finds its siblings by name.

```sh
mkdir aq && cd aq
for r in aq-server aq-worker aq-dashboard aq-git aq-graph aq-playbooks; do
  git clone https://github.com/ElectricJack/$r.git
done
```

`aq-playbooks` is not built from its checkout: the server vendors it
(`vendor/aq-playbooks-5b86e0f`). Clone it to read or change the engine.

## The provider login

Agents and internal work (routing) use your own Claude subscription. Create a
long-lived token once and keep it in a private file:

```sh
claude setup-token                       # prints the token; needs Claude Code installed anywhere
install -d -m 0700 ~/.config/aq-secrets
umask 077; cat > ~/.config/aq-secrets/claude-oauth-token   # paste the token, Enter, Ctrl-D
```

`AQ_LOCAL_CLAUDE_TOKEN_FILE` names another path. The file must be a regular
file with mode 600. `local-stack doctor` checks that it exists and never
prints it. The token is copied to two places inside the stack's own Docker
volumes: the server's credential store (the worker receives it per attempt)
and the internal work host's login volume.

Codex is optional and not set up by this sequence (see [Not done](#not-done)).

## Run it

```sh
cd aq-server
scripts/local-stack all
```

`all` runs these steps in order. Each can be run by itself and run again.

| Step | What it does | Produces |
| --- | --- | --- |
| `doctor` | Checks the tools, the sibling checkouts, the ports and the token file. | |
| `build` | Builds every part from the checkouts (first run: 30 to 60 minutes). | Images `agent-q-server:local`, `agent-q-supervisor-host:local`, `aq-graph:local`, `agent-q-git:local`; `aq-dashboard/dist`; the worker programs in `~/.local/share/aq-local/runtime`; the sandbox daemon; the template image in a loopback registry and `template.json`. |
| `up` | Starts PostgreSQL, aq-graph, aq-git and the server with the dashboard bundle. | Dashboard at <http://127.0.0.1:18080/>. |
| `seed` | Clones a public scratch repository into a bare remote inside the aq-git volume and creates the scratch project. | Project `e0000000-0000-4000-8000-000000000002`. |
| `pins` | Hashes the worker programs and the template image and writes both sides from that one run: the worker's `native.json` and the server's definitions. Restarts the server on them, saves the scratch project's binding, stores the Claude login. | `intelligence.toml`, `node/config/native.json`, `pins.json`. |
| `internal-work` | Starts the internal work host and sets the levels internal work runs on. | Container `aq-local-internal-work-1`. |
| `worker` | Enrolls this machine (once) and starts the worker as the user service `aq-local-worker-local.service`. | Machine `local-dev` on the Swarm page with one node and two agent slots. |
| `task` | Files one small task on the scratch project and follows it to its result. | `PASSED` and the result's conclusion. |
| `routing` | Saves the shipped routing document (`crates/aq-profiles/defaults/routing.md`) when the server has none, waits for the internal work host to author its playbook, switches routing on. | Tasks are routed to a level before they are offered. |

Until `routing` has finished, tasks run on their project's default level
(`standard-high`, Claude Sonnet). Authoring the routing playbook is one long
agent run on Claude Opus; `routing` waits for it and can be interrupted and
run again.

Everything the stack keeps on the host is under `~/.local/share/aq-local`
(state, generated secrets, the worker's enrollment and state).

### A second stack, other ports

Every name and port comes from the environment (the full list is at the top
of `scripts/local-stack`):

```sh
export AQ_LOCAL_PROJECT=aq-two AQ_LOCAL_ADMIN_PORT=38080 AQ_LOCAL_WORKER_PORT=38081 \
       AQ_LOCAL_GRAPH_PORT=38741 AQ_LOCAL_REGISTRY_PORT=5139 AQ_LOCAL_MACHINE_KEY=two-dev
scripts/local-stack all
```

Export the same variables in every shell that runs `local-stack` for that
stack.

### More agents

A node runs as many agents at once as its server Setting allows; a new node
gets the default of two. Change it on the Swarm page, or:

```sh
AQ_LOCAL_AGENTS=4 scripts/local-stack worker
```

`AQ_LOCAL_NODES` (default 1) is the number of node identities enrolled, and is
read only at the first `worker` run.

## Verify

```sh
scripts/local-stack status
scripts/local-stack task --note hello          # one task; ends with PASSED
```

To watch two agents at once, open <http://127.0.0.1:18080/> → Agents → Swarm
and file two tasks together:

```sh
scripts/local-stack task --note one --keep & scripts/local-stack task --note two --keep & wait
```

The worker's log: `journalctl --user -u aq-local-worker-local.service -f`.
The server's: `docker logs -f aq-local-server-1`.

## After a code change

`scripts/local-stack reload PART` rebuilds one part and restarts what uses it.

| You changed | Run | What happens |
| --- | --- | --- |
| aq-server (Rust) | `reload server` | Image rebuilt, server recreated. Database migrations run at start. |
| aq-server `crates/aq-supervisor-host` | `reload supervisor` | Internal work host image rebuilt and recreated. |
| aq-dashboard | `reload dashboard` | `pnpm build`, then the server is recreated (it checks the bundle's manifest at start). For UI work use the dashboard's own `pnpm dev` against this server instead. |
| aq-graph | `reload graph` | Image rebuilt, service recreated. |
| aq-git | `reload git` | Central image and the worker's `aq-git` rebuilt; worker programs re-pinned. |
| aq-worker | `reload worker` | Worker stopped, programs rebuilt, **re-pinned on both sides**, worker started. |
| The sandbox image (`aq-worker/packaging/sandbox`, a harness version) | `reload template` | Template rebuilt and pushed, re-pinned, worker restarted. |
| aq-playbooks | Re-vendor it in aq-server, then `reload server` and `reload supervisor`. | |

### Re-pinning the sandbox driver

The server and the worker both name the SHA-256 of the sandbox driver (a copy
of `aq-worker`), the session host, `aq-agent`, tmux and the template image.
An attempt launches only when both sides name the same digests.

`scripts/local-stack pins` is the one command that pins them. It reads the
programs in the runtime directory and `template.json`, writes the worker's
`native.json` and the server's definitions from the same run
(`scripts/ms1-profiled.py configure`), raises the definitions' revision when
a digest changed, restarts the server and installs the new `native.json` for
the enrolled worker. `reload worker` and `reload template` call it.

`scripts/local-stack check` refuses a mismatched pair and says which side is
stale; `worker` and `pins` run it. Rebuilding the worker programs by hand
(`cargo build` and copying) without `pins` is the mismatch it catches.

## Teardown

```sh
scripts/local-stack down          # stop the worker and the containers; keep all data
scripts/local-stack down -v       # also delete the stack's volumes, its worker unit and its registry
rm -rf ~/.local/share/aq-local    # the stack's host state, for a clean start
```

The sandbox daemon (`aq-sandbox-podman.service`) is shared by every stack of
your user and is left running.

## Troubleshooting

These are the failures hit while this sequence was written.

- **`build template` fails in the runtime image with `couldn't read
  …/packaging/native-runtime/pins.json`.** Your aq-worker checkout predates the
  fix to `scripts/package-sandbox.py` (it must export that file). Update the
  checkout. The template is built from the *committed* worker revision, so
  commit a local change to `packaging/sandbox` before rebuilding.
- **`build template` says the `aqv2` input differs from its pinned SHA-256.**
  `packaging/sandbox/pins.json` in aq-worker pins the aq-server commit the
  in-sandbox `aq` CLI is built from. Your aq-server clone must contain that
  commit (a full clone does; a shallow one may not).
- **A step says `nothing is pinned yet` or `worker and server pins do not
  match`.** Run `scripts/local-stack pins`. It lists which side is stale.
- **The server refuses to start after `pins` with a message about a changed
  body at a revision.** The revision file was lost (`~/.local/share/aq-local/revision`).
  Run `AQ_MS1_REVISION=<a number higher than any used> scripts/local-stack pins`.
- **`development vault differs from generated configuration`.** The stack's
  home was reused with a different port or project name. A stack keeps the
  origin it was first started with: use the same environment, or a new
  `AQ_LOCAL_PROJECT` (and remove the old one with `down -v`).
- **Password authentication errors from the server after deleting the stack's
  home.** PostgreSQL keeps the old password in its volume: `down -v` before
  removing `~/.local/share/aq-local`.
- **`docker exec` or `docker compose exec` prints nothing (Docker Desktop).**
  It happens now and then. Run a read again; do not repeat a write without
  checking its effect.

- **The worker logs `node control: No such file or directory`.** The
  directories `native.json` names as its template `state_dir` and host root
  do not exist. `pins` creates them (`~/.local/share/aq-local/node/state/host`).
- **The worker polls and gets `409 Conflict`; nothing is offered.** The server
  log says why (`docker logs aq-local-server-1 | grep refus`):
  - `native offer admission requirements unavailable`: the owner's
    availability switch is not on. `worker` turns it on; it is also on the
    Swarm page.
  - `configured source observation unavailable or inconsistent`: the scratch
    project follows its remote's `main`, and until AQ has published to it
    once an offer needs a fetch under 30 seconds old. `local-stack task` does
    that fetch; a task filed in the dashboard before the first published
    result waits for it.
- **`routing` says the authoring run failed with "No model that can work with
  tools is set up for writing playbooks" or "intelligence registry is not
  configured".** Authoring started before `pins` had registered the login.
  Run `routing` again: it starts another run.
- **`routing` says the authoring agent asks a question.** Read it under
  Settings → Intelligence → Routing (or `GET /api/settings/routing`). The
  agent stops when the routing document contradicts the Settings it sees, for
  example rules about agents that are not installed. Edit the document or
  answer, then run `routing` again.
- **The server log shows `The playbook engine refused: playbook_invalid_request`
  for the routing document.** The stack was seeded with the older
  `human_review_v2` playbook namespace (`scripts/dev-stack.py seed` without
  `AQ_DEV_PLAYBOOK_PROFILE=host_v9`, which `local-stack` sets). A stack keeps
  its namespace: start a new one.

## Not done

- **The end-to-end run is not finished** (see the status block at the top).
- **Codex and other harnesses.** Only the Claude harness is set up: the
  shipped execution profile and levels are Claude-only, and the internal work
  host gets only the Claude login. The live stack routes with a Codex level
  (`routing-sol-low`); here classification runs on `standard-low` (Claude
  Sonnet) and authoring on `complex-high` (Claude Opus). Both are Settings
  (`AQ_LOCAL_INTERNAL_LEVEL`, `AQ_LOCAL_AUTHORING_LEVEL`, or the Intelligence page).
- **Supervisors.** `scripts/supervisor-service-manager.py` is not started and
  still has the `aq-ms1-supervisor-` prefix hard-coded. Queued work does not
  need it.
- **Other projects.** Only the scratch project is bound. Binding another
  repository still means `scripts/bind-project.py`, pasting the entry it
  prints into the vault's operator section and restarting the server.
- **Roles `writer` and `tester`, and the levels `routing-sol-low` and
  `supervisor-medium`,** are not shipped defaults; `developer` and `reviewer`
  and the nine Claude levels are.
- **Manual (interactive) sessions, the terminal services, the worker install
  gateway and login push** of the live stack are not set up.
- **Installing the sandbox daemon on a machine that has none** is scripted
  (`aq-worker/packaging/local/sandbox-daemon.sh`) but was not exercised: the
  proof machine already ran the daemon. The download's SHA-256 was checked
  against the installed build.
- **Release artifacts.** No versioned image or binary is published; nothing
  here pulls one. The server image still embeds the old dashboard bundle
  (`vendor/aq-dashboard-v0.1.0.tar.gz`); this sequence mounts the bundle built
  from the sibling checkout over it. `Dockerfile.supervisor-host` and the
  sandbox template are built by no release workflow, and `Dockerfile.worker`
  builds a worker source archive far behind aq-worker `main`.
- **The sandbox template is unadmitted**: no credential scan, no allowlist
  entry. That matches the live stack and is for development only.
- **macOS and native Windows** are not supported by the worker.
