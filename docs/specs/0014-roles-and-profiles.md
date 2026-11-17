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
- **A level's id is the name the router uses for it.** Today the router picks
  a class such as `standard-high` (one of simple, standard, complex or deep,
  with an effort), and a separate table in Settings maps each class to a
  profile with some other id (`simple-low` maps to `fast-low`; the two complex
  classes map to generated ids). That table is removed: the level document
  `standard-high` is what a task routed to `standard-high` runs on. The shipped
  levels are `simple-low`, `simple-high`, `standard-low`, `standard-medium`,
  `standard-high`, `complex-low`, `complex-high`, `deep-low`, `deep-high`.
  **(call)**
- **Each level's `description` is what the router reads to choose it.** The
  four descriptions the owner wrote for simple, standard, complex and deep are
  carried into the descriptions of their levels at the switch. **(call)**
- The routing document keeps its own place in Settings and no longer depends
  on the catalog.
- How many agents may run at one level at a time stays a swarm limit, edited
  on the level's page.
- A profile document is its candidates and nothing else by default:

  ```markdown
  ---
  id: standard-high
  name: Standard, high
  description: Ordinary work that touches several parts or needs careful checking.
  ---
  ## Candidates
  | provider | model | reasoning | harnesses |
  |---|---|---|---|
  | anthropic | claude-sonnet-5-5 | high | claude |
  ```

  The selection policy (attempts, fallback reasons, lineage time, cost rules)
  has one set of defaults for all profiles, held in Settings. A profile may
  override single values under an optional `## Selection` heading. **(call)**
- Two values leave the selection policy because the execution profile already
  owns them: the output token limit, and the capabilities a worker must have
  (they follow from the harness and confinement). **(call)**
- Every harness accepts the reasoning value `default`, meaning the harness's
  own setting, so a profile can name a model on any of the four harnesses.

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
- An agent cannot hand out more than it holds. When an agent files a task, the
  role it names (or the project default, if it names none) may only list
  operations that the agent's own role lists. A person or a playbook filing a
  task is not limited this way. **(call)**
- The router never changes the role. Telling the router the task's role, so
  the routing document can say for example that reviews get a stronger level,
  is a follow-up: the facts a playbook may read about a task are a closed list
  in the playbook host contract (aq-playbooks), so it needs a new contract
  version and the routing playbook compiled again. Until then the router sees
  the task's type and content, as today. **(call)**
- A task keeps its existing type (feature, bug fix, docs and so on). The type
  says what the work is; the role says who does it. **(call)**

As built (step 3a):

- The role a task names is task metadata `aq:role`, given as `role` on
  `task.create` and on each task of `task.graph.create`. `profile_hint` is
  gone. One rule gives the effective role everywhere: the task's `aq:role`,
  otherwise the binding's `default_role`. Task reads show it as
  `role: {id, source}` with `source` `task` or `project_default`.
- The owner sets or clears it with `task.role.set` until an attempt of the
  task has been granted.
- A task whose effective role is disabled or no longer exists is held: the
  work read lists `role_disabled` or `role_missing`, and it gets no offer.
- Telling the router the role needs a new host contract version: the
  `work.read` facts are a closed object in `aq-host` 2.2.0 (aq-playbooks), so
  a `role` fact cannot be added from this repository alone.

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
and the caller's authority. `wait.*` operations keep the scope they have today,
the attempt itself. The composer already derives these ids from the
binding for its installation ceiling; this makes that the only source.

### 6.3 What the agent is not told

An agent is told about its own task. Whether it should also see the tasks it
blocks is a separate fault found on 2026-11-16 (an agent did its dependent's
work) and is out of scope here.

## 7. Where the documents live

Roles, intelligence profiles and execution profiles are Settings documents:
stored by the server, edited on Settings pages or through the API, each save
revision-checked. **(call)** Shipped defaults are Markdown files in the
aq-server repository. Each is installed once: a default the owner deletes does
not come back at the next start, and a default added by a later release is
installed at the first start of that release.

On each save the server validates all documents together and, if valid,
activates a new immutable snapshot identified by its content digest. An invalid
save changes nothing and says why. There are no per-entry version numbers and
no `id@version` references.

The documents are plain Markdown with nothing server-specific in them, and can
be exported and imported as files. Owner, 2026-11-16: "We may want to implement
a vault later, but I'm not sure." Nothing here prevents that.

### 7.1 What an attempt ran with

Owner, 2026-11-16: "Can we keep track of past attempt profiles without blowing
up the db and blocking work in the future, like with our various rolling
logs/records?"

Two things are kept, both bounded:

- **A summary on every attempt**: role, intelligence profile, model, reasoning
  effort, harness, execution profile and the snapshot digest. It is a few
  hundred bytes, is part of the attempt record, and lasts exactly as long as
  the attempt does. This is what the task page shows.
- **The full text of each snapshot, stored once per digest.** Many attempts
  share one snapshot, because a snapshot only changes when a document is saved.
  Snapshots are a rolling set (architecture 0006): the active one, every one a
  kept attempt names, and the most recent N others. Activation deletes what
  falls outside that in the same transaction. N is a Setting.

Storage never refuses a save or a launch. When an attempt record rolls off, its
summary goes with it, and a snapshot nothing names any more is deleted. If the
full text of an old attempt's snapshot is gone, the summary still says what it
ran with.

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

- Past attempts keep their results and history. The migration fills each one's
  summary (§7.1) from what its launch record already holds: the model, the
  harness and the old agent name as its role. The old catalog bodies are not
  carried over, so the full prompt text of those attempts is not kept.
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
4. **Documents in Settings or files in the vault.** Answered 2026-11-16:
   Settings documents now; a vault may come later (§7).
5. **Wire rename.** Removing `agent_profile` and `modules` from the launch
   manifest touches about 110 worker files, mostly fixtures. It is the largest
   mechanical step. It is in scope here because of "no old references"; say so
   if you would rather defer it.
6. **History.** Answered 2026-11-16: keep it, bounded (§7.1). Remaining
   point: attempts from before the switch get a summary but not their full
   prompt text. Is that acceptable?

## 12. Work order

1. `aq-profiles`: the three types, document parsing, validation, snapshot.
2. Settings store, routes and shipped defaults for the three document kinds.
3. Tasks carry a role; project bindings and selection: role replaces agent,
   defaults on the binding.
4. Composer and startup: the §6 order, permissions from the role.
5. Removal of the context qualification and model lists (from `feat/any-model`,
   flag removed).
6. Protocol crates and the worker.
7. Migration.
8. Dashboard: Settings pages for roles and execution profiles, labels.
9. Fixtures, scripts and smokes.
10. Rollout and the §10 verification.
