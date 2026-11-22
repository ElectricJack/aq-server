# 0017: Sandbox environments per project and per task

Date: 2026-11-19, revised 2026-11-21. Contract for aq-server, aq-worker,
aq-playbooks and aq-dashboard. Status: draft for owner review. Builds on 0014
(roles and profiles), 0015 (multi-agent nodes), 0016 (delivery by playbook)
and 0019 (integrated delivery). Steps 2 and 3 (§8) are on main behind the
compile-time feature `project_environments` (#206).

The 2026-11-21 revision moves recipes from Settings documents into each
project's repository (§6.1), so a toolchain is reviewed and delivered like
code. That answers the old questions 3 and 4 (§9).

## 1. Intent

Owner, 2026-11-19, on learning that dashboard tasks cannot run their own tests
in the sandbox: "the sandboxes need to be configurable for the tasks. Like some
tasks might require a C++ compiler etc."

Owner, 2026-11-21:

- "We kindof need to be able to have AQ automatically configure worker
  templates for each project. Different projects will have different
  toolchains."
- "In my mind setting up a new project would usually go through the global
  supervisor and I might expect it to create an intial task to intall whatever
  tools are needed in a sandbox, then that image can be saved for that
  project."
- "As projects evolve we may find we need to add additional tools and that
  needs to be possible with tasks."
- On where a recipe lives: "A, recipe in the repo".
- On where builds run: Docker Buildx, because "this system is already in
  progress being deployed with kubernetes to GCP" (in a downstream fork) and
  Buildx is the least work to carry there.

A task must run in a sandbox that has the toolchain its work needs. The
toolchain is a recipe in the project's repository: tasks create and change it,
and it is reviewed and merged like code. AQ builds, scans and admits the image
with no owner hand steps. Adding a toolchain must not need a server release.

## 2. What exists (origin/main, 2026-11-21)

| Piece | Where | What it does |
|---|---|---|
| Template recipe | aq-worker `packaging/sandbox/templates.md`, `aq-worker template build` | A parent image plus `install_steps`, built through `docker buildx build --builder <name> --output type=oci` (`template_tool.py:249`), scanned for credentials, identified by its OCI manifest digest and added to the vault allowlist under a file lock. An explicit operator action. |
| Node template list | node `native.json` `executor.settings.host.docker_local.templates` | The `{digest, image}` pairs a node may run. Already a list (`docker_local/config.rs:42`). |
| Environment | server definition body (e.g. `aq-ms1-docker-local-claude`) | Pins one `template` digest. Since #206 may carry `provides`. |
| Project binding | `project.set` | `allowed_environments` and, since #206, `default_environment`. |
| Placement | `aq-project-policy::environments::place` (#206) | §5.4's default rule, in code until the router outputs `environment`. |
| Task needs | Graph metadata `aq:needs` (#206) | Set on `task.create`, `task.graph.create` and the owner's `task.needs.set`. |
| Build cache | node `build_cache` | Keyed per project and template variant. |
| Delivery | 0016, 0019 | The GitHub App opens, checks and merges AQ's pull requests. Merges are gated by GitHub required checks. AQ's own check runner is off on main (`checks: None`, `playbooks/service.rs`). |

What is wrong:

- The live node runs one hand-built template, `b2851df8…` (2026-11-19). It has
  Rust, and Node and pnpm in `/usr/local/bin`, but no Playwright browsers and
  none of Chromium's system libraries. It is `aq-template-unadmitted`: the
  credential scanner refuses on this host (a GnuTLS failure), so the live
  templates were never admitted.
- Building a template, adding it to the node's list and writing the
  environment definition are hand steps on the stack.
- Nothing turns "this project needs Playwright" into a built environment.

Context qualification no longer applies: 0014 removed it from the server
(`1ffdd416`, "the server holds no context measurement"), so a new template
needs no re-measurement.

## 3. Goals

1. A project declares its toolchain as recipes in its own repository. Its tasks
   run in its default environment.
2. A task can need more than the project default (for example a docs task in a
   Rust project that must render diagrams with Node), and is placed in an
   environment that has it.
3. Any task may add or change a tool, in the same pull request as the code that
   needs it, so code and toolchain never drift apart.
4. A merged recipe change becomes an admitted environment with no owner step.
5. Setting up a new project through the global supervisor ends with the
   project having its own environment.
6. The decision "which environment for this task", and when to build and
   admit, is policy: the routing document and the delivery playbook. Code
   provides facts and operations and refuses impossible placements.
7. Environments stay immutable and pinned. A changed toolchain is a new recipe,
   a new digest and a new environment version.

## 4. Not goals

- Installing toolchains inside a running attempt (no `apt-get` at run time, no
  network-sourced toolchains per task). Task sandboxes keep their confinement:
  `CapDrop: ALL`, `no-new-privileges`.
- Snapshotting an agent's container as an image.
- Per-task custom images. Tasks pick from the project's environments.
- Boat and Cloudflare Containers. Both need one more step per admitted
  template (a Boat saved snapshot, a push to Cloudflare's registry); the
  allowlist's `materializations` field is reserved for it. A later revision.
- Multi-architecture images (linux/amd64 only).

Anti-goals:

- With the feature on, an image that was not scanned and admitted runs a task.
- A credential, login or workspace file ends up in an image.
- A project's broken recipe stops its work. Its tasks keep the last admitted
  environment until a fixed recipe merges.
- A hard-coded limit. Every limit in §6.7 is a Setting.

## 5. Model

### 5.1 Environment gets a capability list

An environment definition gains `provides`: a list of capability names, each a
plain label that a recipe author chooses and documents, for example `rust`,
`node`, `playwright`, `cpp`, `cmake`, `python`. It is a statement about the
template, recorded with it, checked at build (§6.2), and shown in Settings.
There is no global capability registry in code; the labels mean what the
recipes that provide them install. **(call, built in #206)**

### 5.2 Project binding names its environments

The binding's `allowed_environments` stays, and gains `default_environment`
(one of them). A project's tasks run in `default_environment` unless §5.4
places them elsewhere. **(call, built in #206)**

Admitting a project's environment (§6.3) adds it to `allowed_environments`, and
sets `default_environment` when the recipe is the project's default (§6.1).

### 5.3 Execution profile no longer pins the environment

0014's execution profile `environment` field becomes optional. When it is
absent (the normal case) the attempt runs in the environment chosen by §5.4.
When present, it still pins one environment (for profiles that must not vary,
for example a supervisor). Limits, harnesses and credentials stay on the
profile. **(call, built in #206)**

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
`fresh`, PR #202) chooses again. An attempt already placed keeps its digest
when a newer environment version is admitted.

### 5.5 What the agent is told

The environment's platform text (0014 §6.1 step 1) lists its `provides` and
the toolchain versions recorded at build (§6.2), so the agent knows what it
can run and does not try to install what is missing. When a task needs a tool
its environment lacks:

- if the tool belongs in the project, the agent adds it to the recipe in its
  own pull request (§6.1) and proves it with `aq environment try` (§6.2);
- if that is outside its task, it reports `blocked` naming the capability and
  asks its supervisor, which files a toolchain task. The supervisor is the
  point of contact. **(call)**

## 6. Recipes and builds

### 6.1 Where a recipe lives

A recipe is a file in the project's repository, one per environment:
`.aq/environments/<name>.md`. It is reviewed and merged like code (0016). The
environment's id is `<project>/<name>`.

```markdown
---
name: web
default: true
provides: [node, pnpm, playwright, python]
---
## Install
- sh -c "install Node 24.x from the pinned tarball, verify sha256"
- sh -c "corepack enable && corepack prepare pnpm@10.34.5 --activate"
- sh -c "npx playwright@1.64.1 install --with-deps chromium"

## Check
- node --version
- pnpm --version
- npx playwright --version

## Warm
- pnpm install --frozen-lockfile
```

- `Install` becomes the template build's `install_steps` (argv arrays, as in
  templates.md). Steps run as root at build time; that is the only place root
  installs happen.
- `Check` commands run in the finished image at build. Their output is recorded
  as the environment's toolchain versions (§5.5), and a failing check fails the
  build.
- `Warm` is the build-cache warm command (§6.5).
- `default: true` marks the project's default environment; at most one recipe
  in a repository has it.
- A recipe has no `parent`. AQ supplies it: the **AQ base template** (§6.4),
  which holds the sandbox OS, git, tmux, bwrap and the `/opt/aq` runtime
  bundle with the harnesses. The project controls its tools; AQ controls the
  runtime and harness versions. **(call)**
- Install steps must put tools on the agent's PATH, which the worker fixes to
  `/usr/local/bin:/usr/bin:/bin` (`native/adapter/claude.rs:169`). A tool that
  needs an environment variable (for example `PLAYWRIGHT_BROWSERS_PATH`) must
  install where it works without one, or the build check must show the
  variable reaches the agent. **(call)**
- The existing recipe bounds apply (templates.md: steps, arguments, bytes,
  control characters, credential-shaped strings), now as Settings (§6.7).

### 6.2 Trying a recipe from a task

An agent can't install packages in its own sandbox, so it can't test install
steps in place. The operation `environment.try` builds the recipe from the
task's workspace on the builder (§6.6) and runs one command in the result.

- CLI: `aq environment try <name> [--run "<command>"]`, scoped to the agent's
  task and project.
- It returns the build result, the `Check` output, the command's exit status
  and output, and a build log that is bounded and redacted (the build tool
  suppresses build output today because installers can print secrets).
- The image is never admitted, never runs a task and is deleted afterwards.
- This is how a toolchain task proves its work, for example
  `aq environment try web --run "pnpm install --frozen-lockfile && pnpm test:e2e"`.
  The agent records the result on its pull request.

The recipe has not been reviewed when it is tried, so agent-written install
steps run as root inside BuildKit before review. The owner accepted this risk
(2026-11-21); §6.6 limits it.

### 6.3 Build at the pull request, admit at the merge

The delivery playbook decides when to build and admit. The server provides the
operations through the host contract:

- `environment.build`: build a recipe at a commit on the builder, run its
  `Check` commands, scan, and return the digest, toolchain versions and
  result. Runs outside attempt capacity.
- `environment.admit`: add a built digest to the allowlist, write the
  environment definition version (template digest, `provides`, platform text,
  toolchain versions), add it to the project binding (§5.2) and publish the
  digest to nodes.

The default delivery playbook and its Settings document gain:

1. When a candidate changes a file under `.aq/environments/`, call
   `environment.build` for each changed recipe and post the result as a GitHub
   check through the App. It is a required check for those pull requests. A
   failed build goes back to the task's agent like any failed check (0016).
   Building before the merge means a broken recipe never merges. **(call)**
2. The project's own CI stays on GitHub Actions. The proof that the project's
   tests pass in the new image is the agent's `environment.try` run (§6.2).
   Turning on AQ's check runner is separate work. **(call)**
3. On merge (`aq.integration.changed`), call `environment.admit` with the build
   whose canonical recipe bytes equal the merged recipe's. Squash merges make a
   new commit, so the match is by recipe content hash, not commit. With no
   matching build, build first. **(call)**

A failed build or admit leaves the previous environment version in place and
reports the failing step. Nothing is half-registered.

Nodes learn admitted digests from the server, pull the image, and list the
digest in their inventory once they can run it. Until a node lists it, §5.4
does not offer that environment there.

### 6.4 The AQ base template, and when it changes

The AQ base template is built from AQ's own recipe (aq-worker
`packaging/sandbox`) and admitted the same way. A new base (for example a
Claude CLI bump) needs every project environment rebuilt on it, because a
template advertises exact harness versions and placement requires the
selected one (`aq-selection/src/placement.rs`).

- Admitting a new base emits a new host-contract event,
  `aq.environment.base_changed`. The delivery playbook reacts by building each
  project's current recipes on the new base and admitting each that passes.
  **(call)**
- A project whose rebuild fails stays on its previous environment and the old
  harness version, and the failure goes to the project's supervisor, which
  files a task. Other projects move on.
- The old base and the environments built on it stay advertised while any
  project still uses them (§6.7).

### 6.5 Build cache

The node's per-project build cache already keys by template variant. A
project's warm command can differ per environment (Cargo for Rust,
`pnpm install --frozen-lockfile` for web). It moves from the node config to the
recipe's `## Warm`, so it travels with the toolchain. **(call)**

### 6.6 Where builds run

Builds go through Docker Buildx with a named builder, as
`aq-worker template build` does today. The tool needs only an OCI-archive
export; it never loads the image into a Docker daemon. Moving between
deployments is a builder-config change, not a code change.

- **Local stack:** a `docker-container` builder on the host's Docker
  (templates.md: `docker buildx create --name aq-templates --driver
  docker-container --bootstrap default`).
- **The downstream fork on GKE:** a `kubernetes`-driver builder. Buildx runs
  BuildKit as pods, so cluster nodes need no Docker daemon. BuildKit runs on
  its own node pool, rootless if the cluster allows the relaxed seccomp and
  AppArmor that rootless BuildKit needs (GKE Standard does; Autopilot is
  unverified). A Compute Engine VM with Docker, used as a remote builder, is
  the alternative.
- The machine that runs the build tool receives the exported archive (several
  GB for a Playwright or Rust image) and scans it. Run it in the same region as
  the builder and the registry.
- The base images a recipe builds on must be pullable from a registry the
  builder can reach.
- Builds get open network access (apt, npm and the Playwright downloads need
  it) and a build cache separate per project, so a `try` build never reads
  another project's cache.
- An escape from a build reaches the builder's host (the dev machine locally,
  a builder node in the cluster). That is the accepted risk of §6.2.

### 6.7 Settings and rolling records

Settings, not constants: build wall time; build CPU and memory; the recipe
bounds (64 install steps, 32 arguments per step, 4096 bytes per argument,
16 prefetch entries and 1 MiB input today); the `try` log size; and how many
admitted versions to keep per environment and per base. Old versions are
pruned when a new one is admitted, keeping the last N, except a version an
attempt still uses. Nothing finished may block a new build.

### 6.8 A new project

1. The global supervisor creates the project and its binding.
2. It files a first task, "Set up the sandbox toolchain". The task body names
   the owner's request and the repository's own signals: `package.json`
   `engines` and `packageManager`, lockfiles, `rust-toolchain.toml`, and the CI
   workflow's setup steps.
3. The agent writes `.aq/environments/<name>.md` with `default: true`, proves it
   with `aq environment try` running the project's own test commands, and
   delivers it as a pull request.
4. Until that merges, the project's tasks run in the AQ base template.

## 7. The live stack

| Project | default environment | provides |
|---|---|---|
| aq-server, aq-worker, aq-git, aq-graph, aq-playbooks | `<project>/rust` | rust, cpp, cmake, python |
| aq-dashboard | `aq-dashboard/web` | node, pnpm, playwright, python |

The Rust repositories each get the same recipe: what today's template installs
on top of the base.

## 8. Order of work

1. ~~By hand, now: build `aq-dashboard-web` and bind it through a dashboard
   profile and role.~~ Dropped: the live template already has Node and pnpm,
   and §6 replaces the hand path. Playwright waits for step 5.
2. **Done (#206, behind `project_environments`).** aq-server: `provides` on
   environments; `default_environment` on the binding; optional profile
   `environment`; the two holds.
3. **Done (#206).** aq-server: `aq:needs` on task creation and
   `task.needs.set`, shown on task reads.
4. aq-playbooks + aq-server: the router outputs `environment`; the default
   routing document implements §5.4's rule.
5. aq-worker: the credential scanner works on this host (environment repair
   only; any change to what it refuses goes to the owner first). The AQ base
   template is built and admitted from the existing recipe path.
6. aq-worker + aq-server: the recipe format (§6.1), `environment.try` with its
   CLI (§6.2), and `environment.build` and `environment.admit` (§6.3), with
   admitted digests published to nodes.
7. aq-playbooks: the delivery playbook's build check and admit-on-merge
   (§6.3), and the base-change rebuild (§6.4); host contract additions.
8. aq-server: the global supervisor's new-project setup task (§6.8).
9. aq-dashboard: Settings › Environments (recipes, versions, build status,
   `provides`, which nodes can run each); task view shows `aq:needs` and the
   chosen environment.
10. Move `build_cache` warm commands into recipes (§6.5).
11. Pilot on the scratch project, then aq-dashboard, then the Rust projects.
    The feature turns on when every AQ project has an admitted environment.
    Then the hand-built `aq-template-unadmitted` images and their `native.json`
    pins are removed. Until then, projects keep today's image. **(call)**

New code is behind `project_environments`, merged to main, and the flag is
removed when the epic is confirmed.

## 9. Questions for the owner

1. Capability labels are free text chosen by recipe authors (§5.1). Is that
   enough, or should Settings keep a list of known labels?
2. Should `aq:needs` be settable only by filers and the router, or also by the
   agent of a running task (requesting a different environment for its retry)?

Answered 2026-11-21:

- Recipes live in the repository, not in Settings (old question 3).
- A recipe is approved by the pull request's review and merge under the
  delivery playbook, like code (old question 4).
- Builds run through Buildx with a named builder (§6.6).
