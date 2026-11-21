# 0017: Sandbox environments per project and per task

Date: 2026-11-19. Contract for aq-server, aq-worker, aq-playbooks and
aq-dashboard. Status: draft for owner review. Builds on 0014 (roles and
profiles) and 0015 (multi-agent nodes).

## 1. Intent

Owner, 2026-11-19, on learning that dashboard tasks cannot run their own tests
in the sandbox: "the sandboxes need to be configurable for the tasks. Like some
tasks might require a C++ compiler etc."

A task must run in a sandbox that has the toolchain its work needs, and that
choice must be configuration (documents and Settings), not code. Adding a
toolchain for a project must not need a server release.

## 2. What exists (origin/main, 2026-11-19)

The pieces are there; the wiring from "what a task needs" to "which sandbox"
is not.

| Piece | Where | What it does |
|---|---|---|
| Template recipe | aq-worker `packaging/sandbox/templates.md`, `aq-worker template build` | A parent image plus `install_steps`, built with BuildKit on a build node, scanned, and identified by its OCI digest. The digest is added to the vault allowlist. An explicit operator action; the runner never builds. |
| Node template list | node `native.json` `executor.settings.host.docker_local.templates` | The `{digest, image}` pairs a node may run. |
| Environment | server definition body (e.g. `aq-ms1-docker-local-claude`) | Pins one `template` digest, image reservations and premises. Its digest is chained into qualification. |
| Execution profile | Settings document (0014 §5.3) | Names one `environment`, harnesses, credentials and limits. |
| Role | Settings document (0014 §5.1) | Fixes the execution profile (0014 §5.5). |
| Project binding | `project.set` | `allowed_environments` (refuses any other), `ceilings`. |
| Build cache | node `build_cache` | Keyed per project and template variant, so a new template starts a new warm build. |

What is wrong:

- The live stack has exactly one template and one environment. It was built for
  Rust (gcc, g++, make, cmake, python3, cargo/rustc). Node is in the image at
  `/opt/aq/node` but only as the harnesses' pinned runtime, deliberately not on
  `PATH`, with no package cache and no browsers. Dashboard tasks therefore
  cannot run `npm test` or Playwright.
- The environment is reachable only through role → execution profile. A role
  says who does the work, not what toolchain the repository needs, so the only
  way to give the dashboard Node today is a dashboard-specific role and
  profile: a role per toolchain per project, which 0014 set out to remove.
- Building a template, adding it to every node's list, writing the environment
  definition and re-qualifying are all hand steps on the stack.

## 3. Goals

1. A project declares the toolchain its tasks need, once, as a reviewed recipe.
   Its tasks run in that environment by default.
2. A task can need more than the project default (for example a docs task in a
   Rust project that must render diagrams with Node), and is placed in an
   environment that has it.
3. The decision "which environment for this task" is policy: a Settings
   document or the routing playbook decides it. Code provides the facts
   (what each environment provides, what the task asks for) and refuses
   impossible placements.
4. Environments stay immutable and pinned: a task never installs a toolchain
   at run time. A changed toolchain is a new recipe version, a new digest and a
   new environment version.
5. Adding or changing an environment is an owner-approved operation through
   AQ, not a hand edit of the server config or node files.

## 4. Not goals

- Running untrusted recipes. Recipes stay trusted, owner-approved build
  definitions (templates.md). A repository can propose one; it cannot apply one.
- Installing toolchains inside a running attempt (no `apt-get` at run time, no
  network-sourced toolchains per task).
- Non-container confinement changes.
- Per-task custom images. Tasks pick from the project's environments.

## 5. Model

### 5.1 Environment gets a capability list

An environment definition gains `provides`: a list of capability names, each a
plain label that a recipe author chooses and documents, for example `rust`,
`node`, `playwright`, `cpp`, `cmake`, `python`. It is a statement about the
template, recorded with it, checked at build (§6.2), and shown in Settings.
There is no global capability registry in code; the labels mean what the
recipes that provide them install. **(call)**

### 5.2 Project binding names its environments

The binding's `allowed_environments` stays, and gains `default_environment`
(one of them). A project's tasks run in `default_environment` unless §5.4
places them elsewhere. **(call)**

### 5.3 Execution profile no longer pins the environment

0014's execution profile `environment` field becomes optional. When it is
absent (the normal case) the attempt runs in the environment chosen by §5.4.
When present, it still pins one environment (for profiles that must not vary,
for example a supervisor). Limits, harnesses and credentials stay on the
profile. **(call)**

### 5.4 Who chooses the environment

| Input | From |
|---|---|
| The task's needs | Task metadata `aq:needs`: a list of capability names. Set by whoever files the task, or by the routing playbook (from the task's content, type or role). |
| The project's environments | The binding: `default_environment`, `allowed_environments`. |
| What each environment provides | Its definition's `provides`. |
| What nodes can run | Each node's template list (inventory). |

The choice is made where the intelligence level is chosen today, by the router
from the routing document (0014 §5.5), as one more output: `environment`.
The default routing document says: the project's `default_environment` if it
provides every need, otherwise the allowed environment that provides all needs
with the fewest extra capabilities, otherwise hold the task. Owners change
this rule in the routing document, not in code. **(call)**

Code enforces, and only enforces:

- the chosen environment is in the project's `allowed_environments`;
- it provides every capability in `aq:needs` (else the task is held with
  `environment_missing_capability` naming the missing labels);
- the offer goes only to a node whose template list contains the environment's
  template digest (else the task waits with `no_node_has_environment`).

Retries and continuations keep the environment of the attempt they continue
(they reuse its workspace and build cache). A fresh start (owner retry with
`fresh`, PR #202) chooses again.

### 5.5 What the agent is told

The environment's platform text (0014 §6.1 step 1) lists its `provides` and
the toolchain versions recorded at build (§6.2), so the agent knows what it
can run and does not try to install what is missing. A task whose work turns
out to need a capability its environment lacks reports `blocked` naming the
capability. The task's `aq:needs` is then the place to fix it, and the routing
playbook may read that outcome. **(call)**

## 6. Recipes and builds

### 6.1 Where a recipe lives

A recipe is a Settings document, one per environment, for example
`environment/aq-dashboard-web`:

```markdown
---
id: aq-dashboard-web
name: Web (Node 22, pnpm, Playwright)
parent: aq-sandbox-os@sha256:...
provides: [node, pnpm, playwright, python]
---
## Install
- sh -c "install Node 22.x from the pinned tarball, verify sha256"
- sh -c "corepack enable && corepack prepare pnpm@9.x --activate"
- sh -c "npx playwright@1.x install --with-deps chromium"

## Check
- node --version
- pnpm --version
- npx playwright --version
```

`Install` becomes the template build's `install_steps` (argv arrays, as in
templates.md). `Check` commands run in the finished image at build. Their output
is recorded as the environment's toolchain versions (§5.5), and a failing check
fails the build. **(call)**

A repository may carry a proposed recipe (for example `.aq/environment.md`).
AQ never builds from it directly: an agent or the owner opens a Settings
change from it, and the owner approves it like any other Settings document.

### 6.2 Build and registration

Approving a recipe version starts a build job (an operation, not policy):

1. A node with the `template-build` role runs `aq-worker template build` for
   the recipe (outside attempt capacity), then the `Check` commands, then the
   existing scan.
2. On success the digest is recorded with the recipe version, the check output
   and `provides`.
3. The server writes the environment definition version (template digest,
   reservations, `provides`, platform text) and the qualification records that
   depend on it. Today these are the hand steps of a rollout (the definition
   digest chain); this makes them a server operation.
4. Each node pulls or loads the image and reports the digest in its inventory
   when it can run it. Until a node reports it, §5.4 does not offer that
   environment there.

A build failure leaves the previous environment version in place and reports
the failing step. Nothing is half-registered.

### 6.3 Build cache

The node's per-project build cache already keys by template variant. A
project's `build_cache` warm command can differ per environment (Cargo for
Rust, `pnpm install --frozen-lockfile` for web). It moves from the node config
to the environment's recipe (`## Warm`), so it travels with the toolchain.
**(call)**

## 7. The live stack

| Project | default_environment | provides |
|---|---|---|
| aq-server, aq-worker, aq-git, aq-graph, aq-playbooks | `aq-rust` (today's template, renamed) | rust, cpp, cmake, python |
| aq-dashboard | `aq-dashboard-web` (new) | node, pnpm, playwright, python |

`aq-rust` is today's `aq-ms1-docker-local-claude` with `provides` added, so
nothing about running Rust tasks changes.

## 8. Order of work

1. **By hand, now (no code):** build `aq-dashboard-web` from a recipe with the
   existing `aq-worker template build`, add it to the node template list, write
   its environment definition, bind aq-dashboard to it through a dashboard
   execution profile and role. This is the stopgap §2 calls wrong, kept only
   until step 4.
2. aq-server: `provides` on environments; `default_environment` on the
   binding; the execution profile's `environment` becomes optional; holds
   `environment_missing_capability` and `no_node_has_environment`.
3. aq-server: task metadata `aq:needs` on `task.create` / `task.graph.create`
   and `task.needs.set` for the owner before the first grant; shown on task
   reads.
4. aq-playbooks + aq-server: the router outputs `environment`; the default
   routing document implements §5.4's rule. Remove step 1's dashboard role
   and profile.
5. aq-server + aq-worker: the recipe Settings document and the build job (§6),
   including writing environment definitions and qualification as an
   operation.
6. aq-dashboard: Settings › Environments (recipes, versions, build status,
   `provides`, which nodes can run each); task view shows `aq:needs` and the
   chosen environment.
7. Move `build_cache` warm commands into recipes (§6.3).

## 9. Questions for the owner

1. Capability labels are free text chosen by recipe authors (§5.1). Is that
   enough, or should Settings keep a list of known labels?
2. Should `aq:needs` be settable only by filers and the router, or also by the
   agent of a running task (requesting a different environment for its retry)?
3. Recipes as Settings documents (§6.1), with repository files only as
   proposals. Agreed?
4. Who may approve a recipe: owner only, or a playbook gate as for delivery?
