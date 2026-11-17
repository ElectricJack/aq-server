# Roles and profiles: operator's guide

This replaces the profile catalog administration that this file used to
describe. [Spec 0014](../specs/0014-roles-and-profiles.md) removed the catalog,
its activation routes (`/api/profiles/*`), modules, per-project agents and the
context qualification. The file keeps its name so existing links still work.

## The three documents

A task's agent is described by three independent choices. Each is one Markdown
document; the grammar is in the
[`aq-profiles` README](../../crates/aq-profiles/README.md).

| Document | Says | Chosen by |
| --- | --- | --- |
| Role (`developer`, `reviewer`) | The prompt (`## Role`, `## Rules`), the AQ operations the agent may use, and the execution profile it runs under. | The task. A task that names none gets its project's `default_role`. |
| Intelligence profile (`simple-low` … `deep-high`) | An ordered list of candidates: provider, model, reasoning effort, harnesses. Its id is the class the router assigns, and its `description` is what the router reads. | The router. A task that was not routed uses its project's `default_intelligence`. |
| Execution profile (`sandbox`) | Harnesses, sandbox environment, confinement, credential profile per harness, limits. | The role. |

None of them names a project, a repository or a node. Adding a project adds
nothing here.

## Editing

The documents are Settings documents, stored by the server. Edit them on the
Settings pages or through the owner API (human listener, owner role; writes
need `X-AQ-Request: 1`):

| Path under `/api/settings` | GET | PUT | DELETE |
| --- | --- | --- | --- |
| `/roles`, `/intelligence-profiles`, `/execution-profiles` | every document of that kind | | |
| `/<one of those>/{id}` | one document: `markdown`, parsed `fields`, `revision` | create or replace | delete |
| `/selection-defaults` | the selection policy every intelligence profile starts from | replace | |
| `/profiles/limits` | document sizes, snapshots and save receipts kept | replace | |
| `/profiles/snapshot` | the active snapshot digest and the store revision | | |

- A PUT sends `{"markdown": "...", "expected_revision": "<n>", "operation_id": "<uuid>"}`.
  `expected_revision` is the revision last read (`"0"` for a new document).
  Repeating a request with the same `operation_id` returns the first answer.
- Every save validates all documents together. A valid save activates a new
  immutable snapshot, identified by its content digest, in the same
  transaction. An invalid save changes nothing and returns every problem.
- A document that something else names (a role's execution profile, a
  project's default role or level) cannot be deleted while it is named.

## Defaults

The server installs its shipped documents (`crates/aq-profiles/defaults`) at
start. Each is installed once: a default the owner deletes does not come back
at the next start, and a default added by a later release is installed at the
first start of that release. An edited default is never overwritten.

## Snapshots and what an attempt ran with

An attempt launches from the snapshot that was active when it was offered and
stays pinned to it. Two things are kept, both bounded:

- A summary on every attempt (`ran_with`: role, level, provider, model,
  reasoning, harness, execution profile, snapshot digest). It lasts as long as
  the attempt's record.
- The full text of each snapshot, once per digest, as a rolling set: the
  active snapshot, snapshots that live attempts are pinned to, and the most
  recent N others (`snapshots_kept` in `/profiles/limits`, default 20).
  Storage never refuses a save or a launch.

There is no rollback route: to go back, save the earlier text again.

## Models

Any model an intelligence profile names may be offered on a harness a worker
has installed. There is no list of qualified models and no measurement. A
candidate that cannot launch fails its attempt with the harness's own error,
and the profile's fallback applies. `GET /api/settings/model-suggestions`
lists models that discovery has seen per harness; they are suggestions for the
editor and never a gate.

## Binding a project

A project takes work when it has an enabled binding and an entry in
`[profiled_execution] projects`. For any project the server already holds a
binding record for (repository onboarding or the development seed created it):

```sh
scripts/bind-project.py PROJECT_ID                 # show what would change
scripts/bind-project.py PROJECT_ID --apply
scripts/bind-project.py PROJECT_ID --default-role reviewer --default-intelligence complex-high --apply
scripts/bind-project.py PROJECT_ID --pin COMMIT --apply      # or --branch refs/heads/main
```

It checks that the role and level exist, saves the binding (enabled,
`default_role`, `default_intelligence`, ceilings that hold the role's
operations for this project and repository, the execution profile's
environment), and, when the config has no entry for the project or the source
changes, retains the starting commit in central aq-git and prints the entry to
add. The binding has no `catalog_digest` and no `allowed_*_profiles` lists; a
binding that still sends them is refused.

The config entry stays an operator edit, because it is part of the server's
configuration digest:

```toml
[[profiled_execution.projects]]
project_id = "…"
enabled = true
priority = 0
source = {kind = "immutable", object = {format = "sha1", hex = "…"}}
retention_handle = "…"
```

Add it, raise `[profiled_execution] revision` by one and restart the server.
`scripts/ms1-repin-project.py` moves an existing pinned entry of an aq-ms1
stack to the repository's current main.

Per-project instructions go in the binding's `role_notes` (Markdown per role
id, added after the role's prompt), not in a role.

## Removed configuration

`[profiled_execution] context_qualifications` and
`[intelligence] qualified_harnesses[].models` no longer exist. A config that
still has either fails to load with a message naming the key.
