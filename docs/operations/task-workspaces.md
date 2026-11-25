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
