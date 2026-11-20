# Project supervisors own only their project

Date: 2026-11-20. Status: approved by the owner. Follows `2026-11-20-supervisor-full-access-design.md` (PR #220).

## Intent

The owner's target model:

- The global supervisor has owner authority over all projects.
- Each project supervisor has owner authority over its own project only.

PR #220 gave every supervisor owner authority everywhere. The owner accepted that as an interim state; ms1 has run it since 2026-11-20 19:14Z. This change narrows project supervisors and keeps the design simple.

## Approaches

- **Chosen: a project check at the bridge, deny by default.** Each project has its own Graph workspace. Every project-scoped owner request already names its project: reads carry `/projects/{id}/` in the path, task actions carry `payload.project_id`, and `task.create` sends `subject` `{type: "project", id}`. A task id from another project does not resolve in this project's workspace. Checking the named project is therefore enough, with no per-object lookups.
- *Rejected: resolve the target project of every command* (task, attempt, session, result → project). This covers more commands but needs far more code. The owner asked for simple.
- *Rejected: per-command allowlists for project supervisors.* This is the growing-list pattern the phase 1 design ruled out.

## Design (all under `supervisor_full_access`)

1. **Bridge gate** (`aq-supervisor-control/src/owner.rs`). The global supervisor (`permit.project == None`) is unchanged.

   For a project supervisor (`permit.project == Some(p)`), forward only when one of these holds; otherwise return 403 `Project supervisor can act only on its own project`:
   - The path contains the segment pair `projects/<p>` (exact UUID match on the segment after `projects`).
   - It is `POST /api/v1/actions` and the JSON body has `payload.project_id == p`, or `subject.type == "project"` with `subject.id == p`.

   The action body is read once (bounded by the existing 64 KiB command body limit) and forwarded unchanged. A request that names another project, or no project, is refused.
2. **Restore the pre-#220 narrowing for project supervisors** wherever phase 1 removed it:
   - `SupervisorPrincipal::may_enqueue`, which covers `task.create`, `task.graph.create` and the graph actions;
   - `/v1/supervisor/projects` (`project.list`);
   - the `operation.show` owner-journal fallback, which becomes global-supervisor only. Project supervisors recheck an uncertain owner command by resending it, as the instructions already say.
3. **Instructions** (`aq-supervisor-host/src/service/plan.rs`, flag on). The project-scope line becomes: `You supervise only project {label} ({project}); you hold the owner's powers inside it. The server refuses other projects and global administration.`
   - The body stays as in phase 1.
   - The global line is unchanged.

## What a project supervisor can and cannot do

- **Can:** every owner command whose request names its project. That covers its tasks, comments, knowledge, task graph, routes and project settings (project show/set), plus its own `whoami`.
- **Cannot:** requests that name no project or another project:
  - sessions, attempts, results, messages, questions, reviews and decisions;
  - workers, routing, playbooks, global settings, internal work, notifications and supervisors.

  These remain global-supervisor powers. Messages and sessions for its own tasks are a possible later step.

## Non-goals

- Per-object project resolution.
- Changing the global supervisor.
- Phase 2 (read-only code access).

## Acceptance

- A project supervisor can activate, comment on and read tasks in its own project through the bridge and the command endpoint.
- A project supervisor gets 403 on:
  - a task action whose `payload.project_id` is another project;
  - a read whose path names another project;
  - a request naming no project (for example `dashboard.swarm`, `worker.list`, `playbook.source.list`).
- A project supervisor gets the narrowed `project.list`, and cross-project `task.create` is refused.
- The global supervisor's existing bridge tests still pass.
- Flag off is unchanged.
- The aq-server lib line budget holds.
- Delivery is on the same branch and PR #220, then a rebuild and joint redeploy of the server and supervisors.
