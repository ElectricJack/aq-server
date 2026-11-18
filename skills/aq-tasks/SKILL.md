---
name: aq-tasks
description: Inspect AQ Platform v2 work, diagnose routing and dependencies, and create, revise, or hand off tasks using the v2 CLI.
---

# AQ v2 tasks

Use the connection mode in `aq-cli`. Host examples omit the connection flags.
Discover the caller's grants with `aq commands --all`.

## Inspect work

- In a task sandbox, use `aq whoami` and `aq task show self` to identify the
  assignment. Use `aq task children` and `aq task history` when granted.
- As owner, use `aq projects list`, `aq tasks list --project "$project_id"`,
  and `aq tasks show "$task_id" --project "$project_id"`. Follow returned cursors.
- For a task that is waiting, check its state and dependency graph, then inspect
  its routing and attempts as owner. A route, offer, or held slot does not prove
  an agent started. Use session detail for the observed execution state.

```sh
aq task graph show --project-id "$project_id" --task-id "$task_id"
aq task route history --task-id "$task_id"
aq task work --project-id "$project_id" --task-id "$task_id"
aq session show --session-id "$session_id"
```

## File or revise work

Use the project's observed revision when creating work as owner. Set an
explicit schedulable state only when the user wants the work started.

```sh
aq projects show "$project_id"
aq tasks create --project "$project_id" --expected-project-revision "$revision" \
  --title "$title" --body "$body" --acceptance-criteria "$criteria" --task-state draft
```

A granted agent can inspect `aq task create --help` and use `--parent self`
for an authorized subtask. Use `aq run task.graph.create --describe` for a
related set that should be filed together. For edits, activation, dependencies,
or owner retries, check the corresponding command's help and read the current
target revision before submitting. Reconcile revision conflicts by rereading
and reviewing the requested change. Use `aq operation show` in the sandbox or
the owner operation lookup for uncertain mutations before resubmitting.

## Handoff

Use the worker result claim for assigned execution work as described in
`aq-workspaces-and-git`. For an owner closing an active task by hand,
`aq task complete` applies only when no attempt holds the task. Task completion,
retry after a settled attempt, and reopening a task that never had an attempt
have different preconditions. Inspect each command's live help before use.
