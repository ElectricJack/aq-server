# E0 refresh — retained local dashboard acceptance stack

Task `fleet-stone-84` refreshes `aq-e0-final` from the `main` snapshot captured
at build start, source
`c802fed50bd579009b13a4cc11f77af52fe88d22`, which contains G4
`ea28da1c`. The prior server was source
`8f6c49fd91850b924524494e26c5c17d14072796` and image
`sha256:94a3d9adea2cf2f693cde66e8c0b58dd9d9cd3a2b47eced78545092dbefdb638`.
Before the refresh, `/api/v1/overview` omitted `data.sources`, grouped search
returned 404, and bootstrap disabled `provider.quota`.

## Build and retained topology

The build uses `git archive` of the full source revision above, with the committed
lockfile and vendored component inputs. The sole build-recipe adjustment prefixes
`cargo build --locked --release --bin aq-server` with `CARGO_BUILD_JOBS=2`.
The build sets `AQ_SOURCE_REVISION` to the source revision and state format to 44.
The resulting immutable local image ID is
`sha256:6f119d44a482162e7c8d8c04255e56e1e571b9b9e518efd03a8c0fbbd4fd4f96`.
Its OCI revision label matches the full source revision. This image was built
locally, so it has no registry `RepoDigest`; the ID identifies the installed
image content. [Image inspection](evidence/E0-refresh/image.json) and
[build inputs](evidence/E0-refresh/build.json) record the provenance.
No Rust, dashboard, production Compose, or canonical specification changes
are part of this task.

Persistent root: `/home/jkern/.local/state/aq-platform-e0-final`.
The root retains `refresh-build.json`, the source archive, its extracted
`source-refresh-c802fed50bd5/`, the resource-capped Dockerfile, build log,
image inspection, and the original `source/` E0 Compose snapshot.
A private PostgreSQL dump and before-refresh comparison snapshot are retained
there with mode 0600. They must not be published.

The original Compose topology and vault remain in place. The server image is
selected by the root's `compose.refresh.yaml` overlay; the original
`AQ_SERVER_IMAGE` stays pinned for the running Git service. Recreating the server
uses the original two Compose files plus this overlay and
`up -d --no-build --no-deps server`, with explicit existing admin/worker/Graph
ports 32838/32839/32837. No dependency container or volume is replaced.

Admin: `http://127.0.0.1:32838`; worker: `http://127.0.0.1:32839`;
Graph: `http://127.0.0.1:32837`. The stack remains development-only with dev auth.

## Verification

Environment: WSL2 Linux `6.18.33.2-microsoft-standard-WSL2`, Docker 29.3.1,
Compose v5.1.1, Rust 1.96.1, Node v24.15.0, pnpm 10.34.5. Cargo build concurrency
is two; AQ test admission retains its configured worker cap and markers.

G5 source is pinned to dashboard commit
`c0a73996653b2307e19cbcf569d4938f725cda81` from
`aq/steady-cascade-45.6`, supplied by supervisor message
`msg-8c5e77ae0cda42be9fb920675a70bea2`. It was fetched into a separate local
repository under the persistent root, without editing another worker checkout.
`pnpm install --frozen-lockfile` and `pnpm build` pass; the resulting manifest
identifies the exact revision, `dirty: false`, `official: false`, and 30 assets.
The G5 script is unchanged. Its preview proxies `/api` to the actual retained
E0 origin; there is no browser interception or fixture replacement.

The strict pre-refresh command below exits 1 on missing
`overview.data.sources`, reproducing the deployment failure:

```sh
AQ_DAEMON_URL=http://127.0.0.1:32838 pnpm preview --host 127.0.0.1 --port 4196 --strictPort
AQ_LIVE_URL=http://127.0.0.1:4196 \
AQ_LIVE_PROJECT_ID=e0000000-0000-4000-8000-000000000002 \
AQ_LIVE_OUTPUT_DIR=/home/jkern/.local/state/aq-platform-e0-final/refresh-G5-before \
node scripts/probe-core-overview.mjs
```

Both commands run from the persistent root's `dashboard-refresh-c0a7399/`.
[Before-run output](evidence/E0-refresh/G5-before.log),
[before-run result](evidence/E0-refresh/G5-before.json),
[dashboard build](evidence/E0-refresh/dashboard-build.log), and
[manifest](evidence/E0-refresh/dashboard-manifest.json) retain that evidence.

The required `scripts/compose-smoke.sh dev` passes on the new immutable image.
It ran through `aq test .aq/tmp/e0-refresh/test_compose.py -q` with the following
environment (one test passed in 104.77 seconds):

```sh
AQ_SERVER_IMAGE=sha256:6f119d44a482162e7c8d8c04255e56e1e571b9b9e518efd03a8c0fbbd4fd4f96 \
AQ_DEV_GRAPH_IMAGE=sha256:807beba98dcd20b0ae335f09cae8c06ecb182a352ee78de278cb47fccb2ee3be \
AQ_DEV_SMOKE_NO_BUILD=1 scripts/compose-smoke.sh dev
```

The disposable smoke verified service health, UID 10001, private directory and
settlement-file ownership, Graph's non-superuser database role, seed replay and
credential refusal, settlement routes, Git crash/restart, Graph outage/recovery,
and cleanup of its own containers and volumes. Its [log](evidence/E0-refresh/compose-smoke.log)
records the observed results. It did not operate on the retained project.

The retained server alone was recreated successfully on 2026-11-03 UTC
(2026-11-02 Pacific). The original dev helper's `AQ_UPDATER_SPOOL` value is
supplied during Compose interpolation; the resolved development server mounts
exclude the updater paths. Before deployment, the helper checks the image,
development mode, four existing volume targets, and loopback ports.
All four retained services are healthy afterward. The private before/after
comparison passed for the project and binding, Graph task, principal, users,
worker record, seed, credential file hashes, vault configuration hash,
repository and active retention handle. PostgreSQL, Graph and Git retain their
container IDs, start times, restart counts and mounts. Server mounts are unchanged.
The [sanitized retained-state report](evidence/E0-refresh/retained.json) contains
the public identities and installed image; private hashes and credentials are
excluded. In particular, retained base
`2e32645bab743729bfaeb0f2eec417c2ace7794b` remains held by active handle
`e0000000-0000-4000-8000-000000000011`, generation 3, binding generation 1.
The existing Git 0.1.0 process remains pinned; the disposable smoke separately
qualified the new image's vendored Git.

Post-refresh `aq test .aq/tmp/e0-refresh/test_live.py -q` passed both checks
in 13.33 seconds. The first validates actual bootstrap, Overview, grouped search
and provider-usage responses against `protocol/dashboard-read/v1/schema.json`.
The second runs the unchanged G5 strict probe with the same preview/project
environment as the failing baseline, writing to
`docs/implementation/evidence/E0-refresh/G5-live`:

```sh
AQ_LIVE_URL=http://127.0.0.1:4196 \
AQ_LIVE_PROJECT_ID=e0000000-0000-4000-8000-000000000002 \
AQ_LIVE_OUTPUT_DIR=/absolute/path/to/aq-server/docs/implementation/evidence/E0-refresh/G5-live \
node scripts/probe-core-overview.mjs
```

The [strict result](evidence/E0-refresh/G5-live/result.json) is `passed: true`;
the [log](evidence/E0-refresh/G5-strict-live.log) records the same result.
The served dashboard manifest matches the clean local bundle, SHA-256
`a7e84e5f0af4887afb4c3b5557daaab32d8adff46385d1692117aa7c5c3f1be0`.
The probe verified the actual seeded task and its deep link, all five grouped
search kinds, source metadata, desktop task/capacity layout, compact navigation
and no horizontal overflow at 390 and 320 pixels. It observed no page errors,
browser interception or write requests. The
[desktop screenshot](evidence/E0-refresh/G5-live/overview-desktop.png) and
[320-pixel screenshot](evidence/E0-refresh/G5-live/overview-320-viewport.png)
were also inspected.

## Source coverage and acceptance handoff

[Overview](evidence/E0-refresh/overview.json) now publishes nine source records.
The Graph ready count is exactly 1. Unmounted questions, blocked-task and review
owners retain unknown totals (`null`); `needs_you` is also unknown. Partial
worker, result and operation coverage remains explicit.
[Search](evidence/E0-refresh/search.json) returns task, session, result, playbook
and knowledge groups; unknown result/playbook totals remain `null`.
[Bootstrap](evidence/E0-refresh/bootstrap.json) enables `search.read` and
`provider.quota`. [Provider usage](evidence/E0-refresh/provider-usage.json)
returns an empty account inventory because this retained vault configures no
quota accounts. This verifies the mounted quota owner; it does not establish
numeric quota coverage for any provider. No host account credentials were added.
The dashboard correctly displays “No provider accounts reported.”

G5's strict core probe passes against the refreshed E0 origin. Broader G11
dashboard acceptance remains with its dashboard task. The retained server stays
available at port 32838; the temporary port-4196 preview can be restarted with
the command above. Source archives, build outputs, comparison/deployment
helpers and logs remain under the persistent root, independent of this task's
ephemeral worktree. The original seed and secrets remain private there.

Final branch synchronization fetched `main` at
`04eadf0c6e2b1e36999a3be726211bd47909de57`, which adds independent D1a central
delivery work after the frozen build source. That later work is included in the
documentation branch's ancestry; the deployed image remains the precisely
identified G4-containing snapshot above, with state-format label 44.
