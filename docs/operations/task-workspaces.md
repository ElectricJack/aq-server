# Cross-project task workspaces

File the task in the project that owns its acceptance criteria. The task keeps that Graph identity across attempts. Add the sibling projects whose source the task needs, and request a private PostgreSQL instance if its tests create databases.

For example, a terminal repair filed against the server can declare both requirements in its task creation payload. Substitute the actual dashboard project UUID.

```json
{
  "workspace": [
    { "target_project_id": "10000000-0000-4000-8000-000000000002", "access": "write" },
    { "service": "postgres" }
  ]
}
```

A running agent can submit the same body with the scoped `task.workspace.request` command. Submit related requirements together so AQ checkpoints the current attempt once. The request is idempotent for the same inputs. The successor receives the approved workspace and a fresh database. Preserve any SQL setup in migrations or fixtures in source.

## Target project policy

In the target project's existing execution binding, add an entry under `ceilings.permissions`. Set its `scope_reference` to `project:<home-project-uuid>`. Choose the narrowest operation that fits the relationship.

| Operation | Effect |
| --- | --- |
| `workspace.share.request` | The target project owner reviews each request for that home project. |
| `workspace.share.read` | The home project's tasks can read this repository automatically. |
| `workspace.share.write` | The home project's tasks can read and propose changes automatically. |

Combine `workspace.share.read` with `workspace.share.request` to allow reads immediately and require approval for edits. AQ resolves the target's configured source and records its repository identity, binding generation, project revision, and commit. Changing the target's binding invalidates the old authority. A task ref and the target's delivery policy govern publication and review.

The home checkout remains at `/aq/work`; sibling sources appear under `/aq/projects/<target-project-uuid>`. The agent works with source files. The worker handles Git credentials, transport and return of changed files. Use the sandbox-provided `DATABASE_URL` directly. On a development machine with PostgreSQL server tools installed, `scripts/with-test-db COMMAND [ARG ...]` runs a command against its own socket-only cluster and removes the cluster on exit.

## Worker and environment setup

Docker-local workers with node Git and a private state-directory mapping advertise workspace support when polling. The managed worker launcher derives that mapping from its own state bind. When using the Compose worker overlay, set `AQ_WORKER_DAEMON_STATE_ROOT` to the location where the dedicated sandbox daemon can read the worker's journal volume. That path must identify the same underlying directory mounted at `/state` in the worker container. A machine without that mapping can continue to run ordinary tasks.

The server's Git transport currently comes with the `delivery_playbook` build feature. Enable `project_environments` for database-backed tasks. An approved environment that advertises `postgres` must have PostgreSQL server and client tools installed. The template builder checks those binaries, and the sandbox runs a SQL readiness query before starting the harness. Other toolchain requirements use the existing task `needs` and environment `provides` mechanism.

The sandbox daemon must be able to run the environment's unprivileged agent
account. Rootless installations need a UID and GID mapping that includes this
account. Run a task that executes `psql "$DATABASE_URL" -c 'SELECT 1'` when
qualifying a worker environment for PostgreSQL.

For a local server and dashboard from separate checkouts, build the dashboard with `pnpm build` and use `compose.dashboard.yaml` with `AQ_DASHBOARD_BUNDLE` set to the absolute path of its `dist` directory.

## Retained source and delivery

A fresh worker can fetch a pinned commit after its branch moves. AQ first
tries the normal Git checkout. When the commit is absent, AQ serves a retained
bundle for that repository, binding generation and commit. Repeated downloads
reuse the bundle. The source-retention settings `max_bundle_bytes` and
`max_staging_bytes` bound the size of an export and the workspace bundle
reservations. These bundles currently have no automatic eviction. Capacity
exhaustion can delay tasks that need a new bundle.

Before rolling a worker back to a build that predates workspaces, let it finish
its outstanding workspace attempts and reconcile their journal records. The
new worker can read earlier reservations. Older workers reject reservations
that declare `workspace_required`. Preserve the journal if recovery is needed.

After an attempt returns changes, inspect the retained heads for the home
project and its siblings. The owner can record a combined test run and verify
delivery against each changed project's policy. See the
[workspace delivery operations](../implementation/task-workspace-delivery.md)
for request bodies and the conditions that allow the home task to complete.
