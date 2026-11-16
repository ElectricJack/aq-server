# 0014: Roles and profiles (replacing the profile catalog)

**Status:** draft written 2026-11-16 from the owner's direction of that day, for
the owner's review. Nothing is implemented. Design calls made without the owner
are marked **(call)**; open questions are in §11.
**Repositories:** aq-server, aq-worker, aq-dashboard.
**Replaces:** the composition model of
[architecture 0003](../architecture/0003-agent-profiles-harnesses-and-terminals.md)
§2 (agent profiles, behavior modules, capability packs), the catalog
administration in [operations/profile-catalogs](../operations/profile-catalogs.md),
the context qualification of spec 0007 §7.3, and the hidden router profile of
spec 0011. Intelligence selection (spec 0004) and routing (specs 0011, 0013)
otherwise stay.

## 1. What the owner asked for

> I feel like v1's implementation was elegant, easy to use and easy to
> understand.

> I like intelligence profiles, and execution profiles makes some sense to me.
> But I'm not sure why we have separate agent profiles or what modules do.
> Ideally we want fewer combinations of things so there's a smaller number of
> dimensions for agent types to explode into.

> I was always expecting workers to be able to run any model in any harness,
> they are just a container to run agent harnesses like claude/codex/pi/opencode.

> I think we should remove the context window check entirely. It's redundant
> for us to worry about model context windows; the harness can report the error
> or compact.

> I don't want a bunch of old code and tables referencing the old way of doing
> things.

On who chooses the role (2026-11-16, on reading this draft):

> I think the tasks should probably define the role they expect but not the
> intelligence profile. And maybe by default the tasks go to developer roles,
> but we might define review or testing roles as well. We still want the router
> to dynamically assign these to intelligence profiles.

Rollout (owner, 2026-11-16): build it on one branch, verify it on the live
stack, then merge it as a single replacement. No compile-time flag and no
backwards compatibility for this epic.

## 2. What is wrong today

The catalog has four kinds of entry (agents, modules, intelligence profiles,
execution profiles). On the live stack on 2026-11-16:

- Six agents exist. Four generic ones point at a placeholder execution profile
  and cannot run work. The two that run are one per project.
- Eight modules exist. Six are single sentences of 44 to 251 bytes joined by a
  dependency graph. The two real ones are per-project prompts.
- Execution profiles carry permissions with literal project and repository
  ids, so every project needs its own execution profile, its own agent and in
  practice its own module. This is why five of the six AQ projects cannot take
  work without script changes.
- The catalog is one JSON record in the database. No file can be opened or
  edited. Only intelligence profiles are editable, through Settings.
- A model can only be offered work when the server holds a measurement of that
  exact model's context window less than 24 hours old, and each worker holds one
  such measurement. All nine levels therefore run on one model.
- The launch command for Claude ignores the reasoning effort a level asks for.
- No code reads a repository's `.aq/PRIME.md`. An agent sees it only because a
  module's prose tells it to look.

AQ v1 has two concepts: an agent profile (one Markdown file: prompt, config,
tool grants) and an intelligence class (one Markdown file). v1 has its own
duplication, which this spec does not copy: worker profiles are separate per
harness, and each generates a stub per intelligence class.

## 3. Goals

1. A task's agent is described by three independent choices and nothing else:
   a **role**, an **intelligence profile** and an **execution profile**.
2. Each of the three is one document a person can read and edit.
3. Adding a project adds nothing to the catalog.
4. A worker runs whatever model and reasoning effort the offer names, on any
   harness it has installed.
5. A running attempt stays pinned to exactly what it launched with.
6. The old concepts are gone from code, tables, routes, scripts and the wire
   format, not hidden.

## 4. Not goals

- Changing how routing chooses a level (specs 0011, 0013).
- Named, pooled or long-lived sessions (v1 `lifecycle`, `min_active` and
  related fields). v2 schedules through the swarm.
- Role inheritance (v1 `extends`) and per-harness role variants.
- A general tool-permission language. A role lists AQ operations (§5.1).
- Delivery and merge policy (Epic 0).

## 5. The three concepts

### 5.1 Role

What kind of work the agent does. One Markdown document:

```markdown
---
id: developer
name: Developer
description: Makes the change a task asks for and reports evidence.
execution: sandbox
operations: [task.read, knowledge.read, source.write]
enabled: true
---
## Role
You implement the change your task describes, in the workspace you are given …

## Rules
- Change only what the task asks for.
- Run focused checks and report the exact commands and their outcomes.
```

| Field | Meaning |
|---|---|
| `id`, `name`, `description` | Identity and what Settings shows. |
| `execution` | The execution profile this role runs under. |
| `operations` | The AQ operations the role may use. No scope and no ids: the server gives each one the scope of the task's project (§6.2). |
| `enabled` | `false` hands this role no new work. |
| `## Role`, `## Rules` | The prompt. Any other heading is refused, so a typo cannot silently drop text. **(call)** |

A role has no model, no intelligence profile, no harness and no version
number. The router assigns the intelligence profile for each task (§5.5).
Shipped roles: `developer` and `reviewer`. **(call)** Others, such as a tester,
are added by writing a document.

### 5.2 Intelligence profile

How capable a model the work needs. Unchanged in meaning: an ordered list of
candidates (provider, model, reasoning effort, harness) and a selection policy
(attempts, fallback reasons, budget). Changes:

- A candidate states its reasoning effort directly (`default`, `off`, `low`,
  `medium`, `high`, `xhigh`, `max`, as the harness accepts), not through a
  `reasoning_configuration_reference` that has to be looked up. **(call)**
- Any model a profile names is selectable on a harness a worker has. The
  per-harness list of qualified models is removed.
- The hidden `router` profile is removed. A task that was not routed to a level
  uses the project's default intelligence profile (§5.4).
- A candidate that cannot launch (unknown model, no access) fails its attempt
  with the harness's own error, and the selection policy's fallback applies.

### 5.3 Execution profile

Where and how the harness runs. One document, not tied to any project:

| Field | Meaning |
|---|---|
| `id`, `name`, `description` | Identity. |
| `harnesses` | Harnesses this profile may use (`claude`, `codex`, `gemini`, `opencode`). |
| `environment` | The sandbox environment. |
| `confinement` | Container or native, as today. |
| `credentials` | Credential profile per harness. |
| `limits` | Wall time, memory, CPU, processes, startup context bytes, output tokens. |

It has no permissions. The live stack needs one profile (`sandbox`). **(call)**

### 5.4 Project binding

A project names, for its tasks:

| Field | Meaning |
|---|---|
| `default_role` | The role of a task that names none. `developer` unless the project says otherwise. |
| `default_intelligence` | The profile used when the task was not routed to a level. |
| `role_notes` | Optional Markdown per role, added after the role's prompt. It adds; it never replaces (v1's project supplement). |
| `ceilings` | Upper bounds on the execution profile's limits, as today. |

The `allowed_agent_profiles`, `allowed_intelligence_profiles`,
`allowed_execution_profiles` and `catalog_digest` fields are removed. **(call)**
A project that must not use some role or level is a policy matter for its
routing document, not a list on the binding.

### 5.5 Who chooses what

| Choice | Made by | When |
|---|---|---|
| Role | The task. Whoever files it (a person, a playbook, another agent) names the role it expects. A task that names none gets the project's `default_role`. | At filing |
| Intelligence profile | The router, from the routing document. Nobody files a task on a level; a filer may give a hint, as today. | Before the task is offered |
| Execution profile | The role. | Fixed by the role document |

- A task's role is a field of the task, shown on it and changeable by the owner
  while the task has not started.
- Filing a task with a role that does not exist is refused. A task whose role
  is disabled is held with that reason.
- The router is told the task's role along with its type and content, so the
  routing document can say, for example, that reviews get a stronger level.
  The router never changes the role. **(call)**
- A task keeps its existing type (feature, bug fix, docs and so on). The type
  says what the work is; the role says who does it. **(call)**

## 6. What the agent is told

### 6.1 Order

The startup text is assembled by the server, in this order:

1. **Platform text for the environment.** What is true of every task in this
   sandbox: where the workspace is, that the agent does not commit or push, how
   the result is submitted. Owned by the server per environment, not written
   into any role.
2. **The role's `## Role` and `## Rules`.**
3. **The project's notes for that role**, if any (§5.4).
4. **The repository's notes.** See §11 question 3.
5. **The task**: title, body, acceptance criteria and its context.
6. **The result-reporting contract**, as today.

There are no modules, no dependency ordering, and no optional entries dropped
by priority. If the startup text exceeds the execution profile's byte limit the
offer is refused with that reason.

### 6.2 Permissions

The effective permission set of an attempt is each operation in the role's
`operations`, scoped to the task's project (for `task.*`, `knowledge.*`) or the
project's repository (for `source.*`), intersected with the project's ceilings
and the caller's authority. The composer already derives these ids from the
binding for its installation ceiling; this makes that the only source.

### 6.3 What the agent is not told

An agent is told about its own task. Whether it should also see the tasks it
blocks is a separate fault found on 2026-11-16 (an agent did its dependent's
work) and is out of scope here.

## 7. Where the documents live

Roles, intelligence profiles and execution profiles are Settings documents:
stored by the server, edited on Settings pages or through the API, each save
revision-checked. **(call)** Shipped defaults are Markdown files in the
aq-server repository, installed when a document of that id does not exist.

On each save the server validates all documents together and, if valid,
activates a new immutable snapshot identified by its content digest. An invalid
save changes nothing and says why. An attempt records the snapshot digest it
launched with, and that snapshot is kept while any kept attempt names it. Older
snapshots are a rolling set (architecture 0006). There are no per-entry version
numbers and no `id@version` references.

This keeps what the current catalog does well (pinned launches, atomic
activation, rollback) without a separate catalog file format or activation API.

## 8. What the worker receives

A worker is told the harness, model, reasoning effort, limits, permissions and
the startup text, and runs them. It holds no model list and no measurement.

- Removed: the context qualification, the per-worker renderer pin, the daily
  measurement and its sandbox, `service measure`, and the pause states tied to
  them.
- Removed from the offer: token bounds and the context window. The startup
  byte limit stays.
- Claude and Codex receive the reasoning effort. Codex starts with the model
  the offer names.
- The launch manifest names `role`, `intelligence_profile` and
  `execution_profile` and the snapshot digest. `agent_profile` and `modules`
  are removed. This is a new version of the session and admission protocol
  crates, published and vendored as today.

## 9. What is deleted

| Area | Deleted |
|---|---|
| `aq-profiles` | `Module`, `Agent`, module kinds, delivery classes, dependencies, `required_adapters`, `allowed_overrides`, `identities`, the `catalog.toml` loader, `compile_central` |
| Server | `context_budget`'s facts and proof, `context_renewal`, crate `aq-context-renewal`, the measurement route and renewal hold, the measurement holds on the Swarm page, `[intelligence] qualified_harnesses[].models`, `[profiled_execution] context_qualifications`, the `/api/profiles/catalogs*` routes, the router-profile code in `aq-profile-settings` |
| Tables | `profile_catalog_artifacts`, `profile_catalog_registry`, `profile_catalog_operations`, `profile_catalog_environment_templates`, `context_qualification_renewals`; project binding rows are rewritten |
| Worker | Measurement, pin handling, the renderer-qualification comparisons |
| Scripts | The catalog and qualification parts of `ms1-profiled.py`, `remeasure.sh`, the one-off aq-server onboarding script |
| Branches | The `any-model` flag on `feat/any-model` in both repositories. This branch starts from that work and removes the flag and the old path. |

## 10. Rollout

One migration creates the new tables and drops the old ones. It does not carry
old records over.

- Past attempts keep their results and history but lose the link to the exact
  profile they ran with. The live database holds test tasks and one real task.
  **(call)** A database backup is taken first.
- The switch happens with no task running. The server and every worker are
  replaced together, because the launch format changes.
- The six AQ projects are bound again with `default_role: developer`, a
  default intelligence profile and their ceilings. Nothing per project is added
  to the catalog.
- The level profiles are entered again from the current Settings content, with
  each level's own model first.

Verification on the live stack before merge: one task per level on the scratch
project showing the model and effort it ran with; one task on aq-server; one
task on a project that could not run before; a task filed with the reviewer
role; a task filed with no role, which must run as developer.

## 11. Questions for the owner

1. **Role file contents.** Is "prompt, operations, default execution profile,
   enabled" right, or should a role carry anything else from v1's profile?
2. **Role choice.** Answered 2026-11-16: the task names its role, default
   developer; the router assigns the intelligence profile (§5.5).
3. **Repository notes.** Options: (a) the platform text tells the agent to read
   `AGENTS.md` and `.aq/PRIME.md` if they exist, which is what happens today by
   prose; (b) the server reads `.aq/PRIME.md` from the task's source and
   includes it in the startup text, which needs the server to read a file from
   the retained source and has not been checked for feasibility. Recommended:
   (a) now, (b) as a follow-up.
4. **Documents in Settings or files in the vault.** This draft stores them as
   Settings documents, like the routing document. v1 kept them as files that
   took effect on save. Files would need the server to watch and write the
   vault.
5. **Wire rename.** Removing `agent_profile` and `modules` from the launch
   manifest touches about 110 worker files, mostly fixtures. It is the largest
   mechanical step. It is in scope here because of "no old references"; say so
   if you would rather defer it.
6. **History.** Is losing the profile link on past attempts acceptable?

## 12. Work order

1. `aq-profiles`: the three types, document parsing, validation, snapshot.
2. Settings store, routes and shipped defaults for the three document kinds.
3. Tasks carry a role; project bindings and selection: role replaces agent,
   defaults on the binding; the router is told the role.
4. Composer and startup: the §6 order, permissions from the role.
5. Removal of the context qualification and model lists (from `feat/any-model`,
   flag removed).
6. Protocol crates and the worker.
7. Migration.
8. Dashboard: Settings pages for roles and execution profiles, labels.
9. Fixtures, scripts and smokes.
10. Rollout and the §10 verification.
