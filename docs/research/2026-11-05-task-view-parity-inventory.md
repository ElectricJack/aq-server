# Task view parity inventory: v1 → v2 (2026-11-05)

The owner asked for a v1 → v2 parity pass, one dashboard section at a time, starting with the task view. Each section is verified by the owner before the next starts.

This file is the inventory. A read-only subagent built it from:
- **v1:** the task drawer `dashboard/src/panes/task-detail/TaskDetailBody.tsx` and the page `pages/TaskDetail.tsx` in agent-queue2, plus `aq task show`.
- **v2:** the Work view (`aq-dashboard/src/features/work/*`), its actions (`features/actions`), the reads (`api/humanGraphReads.ts`), and the human routes in aq-server (`agent_access/human.rs:83-120`).

Each entry says what the v2 server holds today:
- **exists**: the server has the data, so only dashboard work is needed;
- **partial**: some of it is there;
- **missing**: it needs a new server model or route.

## Unused server reads

The server already serves three reads that the dashboard never calls, all under `/api/human/projects/{p}/tasks/{t}`:
- `/work`: readiness, blockers with `satisfied`, parent, and the attempt and session holding the task;
- `/history`: Graph change history plus up to 16 attempts;
- `/summary`: child counts by state.

## Header and identity

- **Task id and a copy control:** v2 shows the id only in the URL. *Exists.*
- **Title:** v2 has it. *Exists.*
- **Project:** v2 shows it only in the breadcrumb. *Exists.*
- **Priority:** not in v2.
  - It is *partial* on the server: the `task.reprioritize` action exists and writes `aq:priority` metadata.
  - The node read drops metadata, so priority can't be read back.
  - The dashboard has no menu entry for the action.
- **Task type:** not in v2. *Partial*: it is written at create time, with no read and no edit.
- **Labels:** editable in v2 but never displayed. *Exists.*

## Status and why it isn't running

- **Readiness state:**
  - v2 shows only the Graph lifecycle.
  - *Exists* via `/work`: draft, completed, canceled, running, waiting, ready or unknown.
- **Why it isn't running (v1 Explain):** v2 shows placeholder text.
  - *Partial*: `/work` blockers.
  - The replacement and retry refusal reasons are internal only, and a refused retry returns a bare rejection.
- **What it is waiting on:**
  - v2 shows waits per session under Agents.
  - *Partial*: `/api/waits?project_id=` rows carry `task_id`.
- **Held / ingest hold:** *Partial*: `aq:hold` metadata is written but not projected.
- **Provider hold, reroute and undo:** *Missing* per task.
- **Needs attention and dismiss:** shown on the Overview only. *Partial*, with no dismiss.

## Content

- **Description, with editing:** v2 has it.
- **Acceptance criteria:** v2 has it.
- **Deliverables checklist:** *Partial*, through the per-result assessment.
- **Related knowledge and "save finding":** *Partial*. `knowledge.author` exists, but the task read drops task-knowledge edges.
- **Attachments:** *Missing* for tasks; result media exists for results only.
- **Checklist (subtasks):** *Missing*.
- **Collaboration threads:** *Missing*.

## Hierarchy and dependencies

- **Parent:** v2 shows it as a "Part of" row.
  - *Exists* on the server.
  - The adapter drops `parent`, `omitted` and `expansion`.
- **Children:** v2 shows "Contains" rows, capped at 64. *Exists*, along with `/summary` counts.
- **Depends on / Blocks, with status:** v2 has them, but no dependency reason (`edge.description`). *Exists.*
- **Delivery condition and epic delivery:** *Missing* (D1).
- **Adding and removing dependencies:** v2 has it (v1's dashboard didn't).

## Comments

- **Comment history** (author, time, paging) and **add, edit and delete:**
  - *Missing.* There is no task-comment model in aq-server or aq-graph.
  - The nearest things are:
    - result comments (`0044_results_feedback.sql`), which are for results only;
    - task mailbox messages, which are mail to agents;
    - Graph change history.

## Execution and attempts

- **Attempts list:** agent, state, model, class, start and end, outcome, exit reason, link.
  - *Partial*: `/history` attempts give the id, session, whether it holds the task, and status.
  - Per session, `/api/v1/sessions/{id}` gives provider, model, harness and lifecycle.
  - There is no outcome or exit reason per attempt.
- **Retries used / max retries:** *Partial*, internal only.
- **Gates and reviews:** shown in Decisions. *Partial*, filterable by project only.

## Agent and session

- **Assigned agent and session:** *Exists*: `/work` `execution{attempt_id, session_id, status}`.
- **Open the agent's terminal:** *Exists.* `session_id` equals the dashboard session id, so `/agents/:sessionId` works directly.
- **Ask the supervisor why:** *Exists*, via `supervisor.message.send`.

## Code delivery

- **Branch, PR and commits:** *Missing* as a human read.
- **Completion** (summary, checks, evidence): *Partial* via `/api/v1/results?task_id=`, with no commit list or PR.
- **Integration mode and skip-verification:** *Missing*; these are set at project or playbook level.
- **Published, merged and deployed facts:** *Missing* (D1).

## Routing

- **Profile, provider, intelligence class, route source and override:**
  - *Partial*: the create-time hints are stored but not readable.
  - The per-attempt selection is sealed, with no human read.
  - Per session, provider, model and harness are readable.

## Timestamps and cost

- **Created and updated:** v2 has them.
- **Claimed, started and completed:** *Partial*: from session lifecycle, history entries and result submission.
- **Cost and tokens:** *Partial*: from session lineage usage and the observations usage route.

## Actions

- **Stop:** `task.cancel` cancels only the attempt holding the task. Graph state never becomes canceled, and with nothing running the action is rejected.
- **Retry:** `task.retry` works on active tasks only, and a rejection carries no reason.
- **Reprioritize:** *exists* on the server, but the dashboard doesn't expose it.
- **Pause and resume, skip, reopen-with-feedback, delete, and state changes** (activate a draft, complete, reopen): *Missing*.
- **Answer an agent question:** available from Decisions.
- **Edit fields:** `task.edit` covers title, body, acceptance criteria and labels only.
- **Create task:** the UI exposes only title, body, acceptance criteria and initial state. The server also accepts parent, priority, hints and type.
