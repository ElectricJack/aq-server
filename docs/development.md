# Development

## Prerequisites

- Rust 1.96.1 (pinned by `rust-toolchain.toml`; rustup installs it automatically).
- Docker with Compose 2.24.4 or newer (`!override` support). Under WSL2, use
  Docker Desktop's WSL integration.
- Python 3.12 or newer, and `curl` (used by the integration/update scripts).

## Run the stack

```sh
scripts/dev-up
```

This prepares private, gitignored `secrets.dev/` and `state.dev/` directories,
then builds and starts PostgreSQL, aq-graph, a supervised aq-git service, and
the server using `compose.yaml` plus `compose.dev.yaml`. Use `scripts/dev-up -d`
to run in the background. Keep the checkout in the WSL2 Linux filesystem and
enable Docker Desktop integration for that distribution. The admin listener is on
`http://127.0.0.1:8080` and the worker listener on `http://127.0.0.1:8081`,
both bound to loopback only. The generated vault is `state.dev/vault/`, based
on `vault.example/`, with dev auth as `owner@localhost` and graph/Git adapters.
Graph is available on loopback port 8741; its public API requires the private
`secrets.dev/graph_token`. Git exposes only a private Unix socket shared with
the server. Server and Git run as UID/GID 10001. Git state and source staging
share one private named volume at `/var/lib/aq`, allowing atomic renames between
them. The socket, runtime vault and server token copy use separate private named
volumes. A one-shot initializer creates only these project volumes as UID 10001
with mode 0700 and copies configuration/token atomically with mode 0600. It has
no network and exits before runtime services start. It refuses changed retained
configuration or tokens; it does not recursively change source ownership.
The original host vault and credentials remain owned by the host user. Graph
runs as that host UID to read its private token.

The graph build uses revision `225329cd6acf5a775fd2a0b016dd1c45fb16c79b`;
Git uses the same server image, whose retained candidate is pinned by
`vendor/git-candidate.json`. Its explicit Git executable is
`/opt/aq-git-runtime/bin/git`; the shared private paths are `/var/lib/aq/git`,
`/var/lib/aq/source-staging`, and `/run/aq-git/central.sock`.
`AQ_SERVER_IMAGE` and `AQ_DEV_GRAPH_IMAGE` explicitly select prebuilt images
(use immutable digests for reproducible runs). `AQ_DEV_STATE_DIR`,
`AQ_SECRETS_DIR`, and `COMPOSE_PROJECT_NAME` select an independent stack.
`AQ_DEV_ADMIN_PORT`, `AQ_DEV_WORKER_PORT`, and `AQ_DEV_GRAPH_PORT` override
the loopback ports. Existing credentials and generated configuration are
preserved on replay; a differing vault is refused rather than overwritten.
An older development stack using host bind mounts should remain intact: use
fresh state, secrets and a Compose project for this volume layout.

PostgreSQL and the private runtime volumes persist between `scripts/dev-up`
runs. To remove the selected project's volumes (this deletes its dev data):

```sh
python3 scripts/dev-stack.py down -v
```

PostgreSQL only reads the password when it first initializes the volume. If you
delete or regenerate `secrets.dev/postgres_password` while an old volume exists,
the server fails with password authentication errors until you reset the volume
as above.
The host seed receipt and saved worker credential also belong to the old
database; preserve them and choose fresh state/secrets for a clean initialization.

Cloudflared is disabled in development (it sits behind the `tunnel` profile).

To check readiness from the loopback-only endpoint inside the server:

```sh
python3 scripts/dev-stack.py readiness
```

Export the paths printed by `scripts/dev-up --init-only` if invoking Compose
directly. The readiness command reports `dependencies.graph` and
`dependencies.aq_git` as `ready` or `degraded`, and exits nonzero when either
dependency or the database is unavailable. It invokes `aq-server dependency-readiness`
against the loopback-only `/dependency-readyz` surface. The updater's `/readyz`
and `aq-server readiness` retain their original database-only report and schema.
Graph uses its own `aq_graph` database and non-superuser role, with a separate
private password; the server never reads its tables. An existing Graph database
with a different owner is preserved and refused; use a fresh Compose project.
Git's supervisor restarts a crashed
`serve` child after five seconds; Compose restarts the supervisor if it exits.
The pinned Git process contains a guard and a serving engine. Engine failure
is recovered by the guard and supervisor; loss of the guard or entire container
may leave a Git recovery fence and require the component's explicit operator
recovery procedure. The supervisor never clears a recovery fence automatically.

## Seed the designated scratch project

The owner must choose the GitHub repository and base branch. No repository,
branch, or credential reference is selected by default. After starting the stack:

```sh
export DEV_SCRATCH_REPO_URL=https://github.com/OWNER/DESIGNATED-SCRATCH.git
export DEV_SCRATCH_BASE_BRANCH=OWNER_DESIGNATED_BRANCH
export DEV_SCRATCH_CREDENTIAL_REF=none
python3 scripts/dev-stack.py seed
```

`none` is an explicit choice for a public repository. The pinned Git candidate
rejects named credential profiles; private authenticated remotes require a
separately qualified candidate. URLs containing credentials are rejected.
The seed registers the remote/base ref through Git, creates one graph task,
activates the checked-in example profile artifact, and creates one revisioned
project binding through the public owner API. It does not fetch or publish.
Stable IDs and operation receipts make retries safe. Changing seed inputs
is refused; use a new state directory **and** Compose project to start fresh.
For nondefault ports, set `AQ_DEV_SERVER_URL`, `AQ_DEV_WORKER_URL`, and
`AQ_DEV_GRAPH_URL` explicitly.

After the seed succeeds, the command enables the local human settlement owner
and restarts the server gracefully. `python3 scripts/dev-stack.py settlement`
replays that activation independently after a completed seed. PostgreSQL exposes
a real Unix socket through a named volume at `/run/aq-postgresql`; its directory
is owned by PostgreSQL with mode 0755 and the server mounts it read-only. The
initializer creates a non-superuser `aq_playbook` role owning only the
`aq_playbooks` engine schema in the same `agent_q` database as the server. A
separate database would fail the supported runtime's same-database guard.
The role also receives database `CREATE`, required by the engine's startup
`CREATE SCHEMA IF NOT EXISTS`; it receives no grants on AQ's public tables.
Fresh databases require SCRAM authentication for local connections as well.

The generated `human_review_v2` vault lives in the runtime volume at
`/vault/settlement/config.toml`. The host `state.dev/settlement.json` pins the
actual owner UUID, namespace, socket UID, grant UUID and a fixed 23-hour grant
deadline. Independent engine and bridge keys and the engine password are
generated once under `secrets.dev/`. The initializer copies the DSN, keyring,
raw bridge key and compiled artifact into server-owned mode-0600 single-link
files; their values never appear in the generated Compose environment.
Replay preserves these pins and never renews an expired or revoked grant.
Changed retained files are refused; use fresh state, secrets and a Compose
project for a new configuration.

The checked-in `scripts/dev-playbook-artifact.json` is generated by
`cargo run --locked --offline -p aq-server --example development_inert_playbook`.
It is a labelled inert definition with no commands, triggers or automatic
admissions. Its grant permits only `operation.get` for the seeded task. This
activates the supported decision and settlement HTTP owners without creating
fake reports, approvals or completed tasks. A real retained result and human
review are still required to settle work.

The example catalog requires intelligence configuration. The seed enrolls an
offline `e0-local-development` worker because binding validation requires a real
worker identity. Its credential is kept in private `secrets.dev/dev_worker.json`
and never printed. New credential files are published atomically. Replay checks
the saved node identity and authenticates it with a correlated worker probe,
without marking the worker online. Missing, malformed or stale credentials are
refused while preserving identities. Recover the original credential, or use
fresh state, secrets and a Compose project; seed does not silently enroll a
replacement node. Configure an actual worker and revise the profile policy for MS0/MS1. E0
establishes local service integration, not a model-backed execution claim.

Run the disposable acceptance test without a designated GitHub repository:

```sh
scripts/compose-smoke.sh dev
```

`compose_dev_reaches_ready_with_graph_and_git` uses its own local bare Git
repository, project, state, ports, containers and database volume. It verifies
UID 10001, private directory ownership and seed replay, kills the Git child,
observes degraded readiness and a new healthy
process, then tests graph outage/recovery and removes its Compose resources.
It also activates and restarts the settlement owner, checks retained pins and
private file ownership, reads the real empty decision inbox, and checks the
reconcile owner's typed absent-completion and malformed-body refusals. This
fresh-stack smoke qualifies route availability; successful completion of a
reviewed execution remains the I0 integration scenario.
Explicit preloaded server/Graph image inputs can avoid rebuilding those candidates.

## Run a worker from source

```sh
scripts/dev-worker
```

With no worker config yet, it prints how to enroll:

```sh
curl -fsS -X POST -H 'X-AQ-Request: 1' http://127.0.0.1:8080/api/enrollments
scripts/dev-worker enroll --server http://127.0.0.1:8081 --name "$(hostname)"
```

Paste the returned `code` when prompted. Run `scripts/dev-worker` again to
connect and heartbeat. `GET /api/workers` shows the worker as `online`.

The command verifies and extracts the immutable source candidate described by
`vendor/worker-candidate.json`, then runs its independent Cargo workspace. It
does not read a sibling checkout. `AQ_WORKER_BIN=/absolute/path/to/aq-worker`
explicitly selects a development executable; the command prints that override.
Official artifact checks reject local executable/source overrides.

## Simulating another user

Dev auth treats every request as the vault's `dev_email`. To act as a viewer
(or any other user), send the `X-AQ-Dev-Email` header:

```sh
curl -H 'X-AQ-Dev-Email: viewer@example.com' http://127.0.0.1:8080/api/me
```

Dev auth is refused in production: the server will not start with
`environment = "production"` and `auth.mode = "dev"`. In addition,
`compose.yaml` sets `AQ_DEPLOYMENT=production` on the server. With that
variable set, the server exits with status 2 unless the vault has
`environment = "production"` and a non-dev `auth.mode`, so a development vault
such as `vault.example/` cannot be mounted into the production stack by
mistake. `compose.dev.yaml` overrides it to `development`.

## Tests

```sh
eval "$(scripts/test-db)"   # disposable PostgreSQL on 127.0.0.1:55432
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Database migrations

`crates/aq-server/migrations/` is the migration manifest. Its SQL filenames,
sorted lexically, must be exactly `0001_description.sql`, `0002_description.sql`,
and so on, with four decimal digits, a lowercase description, unique numbers,
and no gaps. Cargo's `build.rs` rejects duplicates, gaps and invalid ordering
before compiling the server. CI also runs `python3 scripts/migration_catalog.py`
and the focused catalog regressions.

Coordinate the next unused numbers before parallel work: reserve distinct
consecutive numbers and integrate in that order. Two branches adding, for example,
`0061_first_change.sql` and `0062_second_change.sql` only add their own files;
merging them needs no registry edits. The second branch's catalog check will
reject the missing 0061 until it includes the first migration. If both branches
choose 0061, the combined catalog fails CI even when Git merges their different
filenames cleanly. Renumber only an unpublished, unapplied migration after
coordination. Never rename, renumber or edit a migration already applied to a
database; add a new migration instead.

Each migration must set `meta.state_format_version` to its number in the same
transaction. `store.rs` registration, `STATE_FORMAT_VERSION`, and
`READABLE_STATE_FORMATS` are generated from this directory at build time; do not
edit them when adding a migration. SQL remains embedded in the binary, and Docker
copies the entire crate tree. No generated registry is checked in.

Server image labels use the same catalog. Release, dev-stack and smoke entrypoints
derive the build argument automatically. For a direct Docker build use:

```sh
docker build --target server \
  --build-arg AQ_STATE_FORMAT="$(python3 scripts/migration_catalog.py)" .
```

For direct development Compose builds export
`AQ_STATE_FORMAT=$(python3 scripts/migration_catalog.py)` first. Docker's server
build rejects a missing or stale value against the compiled catalog, so the image
label always agrees with the release manifest. No numeric Dockerfile default
needs to change when adding a migration.

## Smoke test

```sh
scripts/smoke
```

Builds the daemon and pinned standalone worker from a frozen source snapshot,
then starts a uniquely named disposable Compose project with Docker-assigned
loopback ports. It enrolls the worker, verifies heartbeat/probe/protocol
rejection, restarts the daemon and database, checks the same saved node identity
reconnects, and revokes the credential. The worker must exit with code 3.
Existing development services can remain running.

`AQ_SMOKE_SERVER_IMAGE` and `AQ_SMOKE_WORKER_IMAGE` select a prebuilt pair.
`AQ_SMOKE_OFFICIAL=1` requires both to be digest references and rejects local
worker overrides. The worker's embedded source manifest must match the selected
candidate. `AQ_SMOKE_EVIDENCE_DIR` selects the retained log/identity directory;
otherwise its temporary path is printed. Disposable resources are removed.

```sh
scripts/execution-smoke
```

This uses the same assembly and additionally runs the real standalone runtime
with a deterministic child. It cancels an offer before acquisition, executes a
different offer, loses acceptance/report responses, checks one launch and a
durable accepted result, replays the report, and restarts the daemon. Synthetic
assignment creation is development-only. This fixture does not certify graph
completion, real model harnesses, hosted access or host-power-loss recovery.

```sh
scripts/recovery-smoke
```

The 20-case recovery matrix builds the pinned worker and a separate daemon image
with `recovery-test-support`. It injects launch-boundary crashes, transport loss,
expiry, cancellation races, database lock waits and bulk saturation through
observable barriers. A historical worker also attempts to read journal format 2.
`AQ_RECOVERY_EVIDENCE_DIR` selects retained evidence; owned containers and volumes
are removed. The production daemon image excludes these clock/load controls.
This qualifies the cooperative Linux guardian profile for runtime-process
restart. Guardian loss retains uncertainty; whole-container reattachment and
physical suspend/power-loss remain separate work.

Run the additional daemon regressions with
`cargo test --workspace --features recovery-test-support --locked` against the
disposable test database. The [agent graph contract](../protocol/agent-access/v1/README.md)
has a separate offline conformance suite with pinned developer dependencies.
CI runs the contract suite and container recovery matrix as separate jobs, and
keeps the ordinary daemon tests alongside the recovery-feature regressions.

## Update lifecycle suite

```sh
scripts/update-lifecycle
```

Proves the host updater against real images, a real registry, the production
`compose.yaml` and real PostgreSQL. It builds `aq-updater` with the
`test-verifier` feature in a private target directory, starts a uniquely named
throwaway `registry:2` on a Docker-assigned loopback port, and builds and pushes server
images for 0.1.0, 0.1.1 and a broken 0.1.2, each with a manifest signed by a
throwaway test key and its `release` image. Under the Compose project
a uniquely named disposable project with a production-shaped vault it checks, through
`aq-updater status` and `aq-server readiness`: a first install, an upgrade, a
rollback of the broken release, recovery after `kill -9` mid-update, refusal of
a release that cannot read the installed state format, refusal of a downgrade
requested through the spool, and that the database survives all of it.
It also starts from a fresh host: the updater's first `run --once` creates the
spool and a `release.env` with a placeholder image, which is what lets Compose
start postgres before any release is installed. The independently pinned worker
candidate remains unchanged across these synthetic server versions. A future
state-format refusal always tests the current format plus one. Disposable
resources are removed on exit.
It ends with `update-lifecycle: PASS` or names the failed step; on failure its
logs are copied to a kept `${TMPDIR:-/tmp}/aq-update-lifecycle-logs.*`
directory, whose path it prints. Public manifests, runner/image identities and
logs are also retained on success (`AQ_UPDATE_EVIDENCE_DIR` selects the destination).
The first run compiles all dependencies;
later runs reuse BuildKit cache mounts.

## Releases and operations

[`docs/operations.md`](operations.md) covers the CI, release and promote
workflows, cutting a release, and installing and running the host updater
(`aq-updater`), including what to do when an update needs an operator.

`scripts/smoke` can also test prebuilt images instead of building: set both
`AQ_SMOKE_SERVER_IMAGE` and `AQ_SMOKE_WORKER_IMAGE` (tags or digest
references; missing ones are pulled). The release workflow uses this to smoke
test the exact pushed digests.

## Production notes

`compose.yaml` is the production shape: no published ports, a read-only vault
mount, the updater spool from `$AQ_UPDATER_SPOOL` (`requests/` read-write at
`/updater/requests`, `status/` read-only at `/updater/status`), and secrets from
`$AQ_SECRETS_DIR` (`postgres_password`, `tunnel_token`). To check it:

```sh
AQ_SERVER_IMAGE=example/agent-q-server@sha256:$(printf '0%.0s' {1..64}) \
AQ_VAULT_DIR=/tmp/vault AQ_SECRETS_DIR=/tmp/secrets AQ_UPDATER_SPOOL=/tmp/spool \
  docker compose -f compose.yaml config
```

Compose bind-mounts secret files with their host permissions, so they must be
readable by the container users (the server runs as UID 10001). The server
entrypoint exports the database password as `PGPASSWORD` from the secret file
and uses a password-less `DATABASE_URL`, so the password needs no URL encoding.
The image sets `AQ_VAULT_DIR=/vault`; `aq-server healthcheck` reads the vault to
find the admin port.
