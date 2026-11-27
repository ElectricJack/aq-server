---
name: aq-workspaces-and-git
description: Find your AQ v2 task workspace, inspect Git changes, hand off results from a managed sandbox, or publish from an authorized host checkout.
---

# AQ v2 workspaces and Git

## Identify the checkout and assignment

Run these commands in the checkout you are working in. A blank branch name
means Git has checked out a detached HEAD. Use the commit ID to identify it.

```sh
git rev-parse --show-toplevel
git branch --show-current
git rev-parse HEAD
git status --short
```

With `AQ_CHANNEL` set in a managed AQ v2 task sandbox, run `aq whoami` to get
the workspace, task, attempt, and session IDs and the current grants. Use
`/opt/aq/bin/aq` if the sandbox command is missing from PATH. The worktree
path and HEAD come from Git. Stay in the assigned checkout and follow its
task and review instructions.

## Request another project's source or a test database

In a managed task, check `/aq/projects/<project-uuid>` for an assigned sibling
checkout. Read-only checkouts support builds and tests. Make source changes
only in a checkout granted write access. The worker captures each writable
checkout when the attempt stops. Use the sandbox's `DATABASE_URL` for SQL
tests when a private PostgreSQL service was requested.

If a required checkout or database is missing, use
`aq run task.workspace.request --params-file FILE` from the task sandbox.
For example, substitute the dashboard project UUID in this request file:

```json
{"workspace":[{"target_project_id":"10000000-0000-4000-8000-000000000002","access":"write"},{"service":"postgres"}]}
```

Request related resources together. Save edits to the assigned writable
directories before sending the request. AQ checkpoints the attempt and resumes
the same task after approval on a capable worker. Keep database setup in source
fixtures or migrations so the next attempt can recreate it.

## Hand off work from a managed task

Review the diff, staged changes, and untracked files. Run the required checks.
Keep only intended changes in the workspace. The worker captures the Git
workspace when the attempt stops, including tracked changes and non-ignored
untracked files. Local commits are optional unless the assignment requires
them. Preserve the assigned base as an ancestor of the work.

Follow the startup instructions for reporting the result. When
`AQ_AGENT_RESULT_DIR` is present and the work is ready, write a private request
file at the path supplied by the worker with this JSON shape. Give a one-line
conclusion that summarizes the work and verification, up to 512 bytes.

```json
{"schema_version":1,"conclusion":"Updated the validation path and passed the focused tests."}
```

```sh
aq result submit --request-file "$AQ_AGENT_RESULT_DIR/submissions/request.json"
aq result status
```

If submission returns pending or uncertain, use `aq result status` to inspect
the original claim before retrying. If a retry is appropriate, reuse the
original request. Result capture and publication have separate outcomes.
Delivery through AQ, including any branch or PR publication, follows the
project's server-side policy. A recorded local claim alone does not establish
that publication occurred. For a manual session without a worker result
outbox, follow its assignment or the authorized host Git workflow.

## Work from a host checkout

For local implementation, use ordinary Git worktrees, branches, and commits.
Use the repository's configured remote and authenticated GitHub client when
publication is authorized. Open a PR against the requested base using
`gh pr create` or the connected GitHub app. A host checkout and its remotes
are separate from an AQ-managed task workspace.

For a managed attempt, the owner can use v2 reads to look up its session and
execution state, then check recorded results by project. Use the admin origin and its Access
token file for an HTTPS deployment. These examples target the local
`aq-ms1` development stack.

```sh
aq --local --origin http://127.0.0.1:28080 session list --limit 10
aq --local --origin http://127.0.0.1:28080 session show --session-id "$session_id"
aq --local --origin http://127.0.0.1:28080 result list --project-id "$project_id"
```

Use `aq commands --all` with the appropriate connection flags to check
available commands and grants. The v1 daemon's workspace administration and
guarded Git commands belong to the legacy CLI, `aq1` on a dual-install host.
