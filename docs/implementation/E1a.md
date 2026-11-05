# E1a production Compose evidence

Task: `smart-falcon-16.2`. Completed 2026-11-02 (America/Los_Angeles).

The production stack includes PostgreSQL, `server`, E2 graph, supervised A3b
Git and cloudflared, all selected by immutable digest. Production Access auth
stays enabled. Database owners are separate non-superusers; runtime services
have no host ports, read-only roots, dropped capabilities and rotated logs.
The one-shot state initializer has only the ownership capabilities needed for
project volumes. Operations and the production vault template document setup.

## Environment and source

- Linux `6.18.33.2-microsoft-standard-WSL2`, Docker Engine `29.3.1`,
  Docker Compose `v5.1.1`, Python `3.12.3` through `aq test`.
- Recorded task base: `8079f7c1f1fcedc53086a0de835bf1ef5ad6f61e`.
- Verified runtime implementation: `99c01aa193720dc3116821cc407b07f17926cd53`
  (tree `4a9d8634eb518456d4530bcadd03f0f0a2a59aa3`). The production smoke
  began at HEAD `23859b221c5e62f272d51f03ee8d56afa64bbe62` with the two final
  script/mount fixes present in its working tree; those exact fixes were then
  committed as `99c01aa1`. No Rust source or Cargo manifests changed.
- The final publication squashes the task's own commits while preserving
  upstream ancestry. Its full SHA is recorded in the AQ publication/close receipt.
- E2 graph: `ghcr.io/electricjack/aq-graph@sha256:9978ccdd8aa6ccd00d80521a1bb791fbf73f1eaae1ddd9b0ae8ee869be071707`,
  v0.1.1, source `2d5b02c610b53baec03658171e1d6e6ebece4bd2`.
- A3b Git binary SHA-256 verified inside the running Git container:
  `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2`.

## Commands and results

All commands ran from the assigned aq-server worktree. Each check below passed.

```sh
scripts/compose-smoke.sh prod
```

[Production output](E1a-checks/production-smoke.log):
`PASS compose_prod_reaches_ready_with_graph_and_git`. The standalone command
built this checkout and pushed it to a disposable anonymous loopback registry,
then used manifest digest
`sha256:2149fa796fc1fb7c8cf9e119c36460ec87724a07ec57562f721ac615792144c0`.
The fixture disables only cloudflared. Live readiness reports
`auth_mode=cloudflare_access`, database OK, graph ready and Git ready.

The smoke checks both database owner identities and non-superuser status,
denied cross-owner CONNECT, successful initializer replay and refusal after
introducing CREATEDB privilege drift. The graph bootstrap token is
`10001:10001:600`. Stopping each dependency degrades readiness, restarting it
restores readiness, and killing the actual Git serve engine produces a new
supervisor child and restores readiness. Cleanup verifies that the disposable
project's containers and volumes are removed; the registry and image are removed.

```sh
AQ_SERVER_IMAGE=aq-e1a-dev:smart-falcon-16.2 \
AQ_DEV_GRAPH_IMAGE=sha256:807beba98dcd20b0ae335f09cae8c06ecb182a352ee78de278cb47fccb2ee3be \
AQ_DEV_SMOKE_NO_BUILD=1 scripts/compose-smoke.sh dev
```

[Development output](E1a-checks/development-smoke.log):
`PASS compose_dev_reaches_ready_with_graph_and_git`. The temporary server tag
reused the production build image
`sha256:0d594567ae390e4744e9a1b48a143e0bfde2074c0e9129b03f2350451dc75bf4`
and was removed afterward. This also exercises seed setup, credential replay
refusals, private ownership, settlement files, graph outage and Git recovery.

```sh
aq test scripts/test-compose-prod.py scripts/test-dev-stack.py
CARGO_BUILD_JOBS=2 cargo clippy --locked -p aq-server --lib --bins -- -D warnings
cargo fmt --check
python3 -m py_compile scripts/compose-prod-smoke.py scripts/compose-smoke.py scripts/product-smoke.py scripts/test-compose-prod.py
sh -n scripts/prod-db-init.sh scripts/prod-init-volumes.sh scripts/dev-git-supervisor.sh
bash -n scripts/compose-smoke.sh
git diff --check
```

[Focused tests](E1a-checks/focused-tests.log): **19 passed** after the latest
instruction merge. [Scoped Clippy](E1a-checks/clippy.log): passed with warnings
denied. Formatting and syntax checks passed. The focused tests cover production
auth/network/image configuration, development overlay boundaries, the older
product smoke's standalone database fixture, invalid digest refusal, unsupported
database owner refusal and the E2 private bootstrap token mount.

## Test-first failures and review

The named production acceptance was added first. Its
[initial negative control](E1a-checks/initial-red.log) failed on unpinned
PostgreSQL before startup. The
[private token regression](E1a-checks/private-token-red.log) failed before the
mount fix. Runtime testing also exposed E2's strict token mode requirement and
psql 17 returning success for the attempted `\\quit 1` refusal. The final
implementation uses the initialized private token volume and SQL RAISE EXCEPTION
under ON_ERROR_STOP; the passing smoke verifies both behaviors.

Two earlier registry attempts failed before stack startup because Docker Desktop
published-loopback addressing and the WSL credential helper were unavailable.
The fixture now uses the engine host's loopback and an empty client config for
anonymous local push. Those attempts are not counted as passes.

Self-review reconciled the task scope, all service pins and the dev/product
overlay changes. This task has no adversarial-review requirement or PR gate.
The project prime explicitly requires `origin/main` in place of the nonexistent
`origin/dev`; final synchronization and guarded publication use that policy.

## Limits

This is disposable local stack evidence. It does not exercise a real Cloudflare
tunnel, external Access token validation, VPS installation, backup/migration,
dashboard image packaging, worker services or updater format 2; those belong to
the named follow-on tasks. Production auth mode remains enabled throughout the
smoke. The older product smoke was checked for Compose configuration compatibility,
not run end to end. Per AQ's focused-validation policy, no full workspace test
suite was run. Existing installations using the old `agent_q` server database
need an explicit migration; initialization does not migrate retained data.
