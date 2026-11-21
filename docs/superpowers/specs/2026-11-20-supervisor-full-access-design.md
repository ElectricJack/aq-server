# Supervisors get full access (phase 1: every owner command)

Date: 2026-11-20. Status: draft for owner approval.

## Why

Supervisors are meant to be the owner's super users: the point of contact that
can do almost anything the owner can. Today they can call 21 of the 154
registry commands. The global supervisor's chat (cursors 4 to 22) shows every
refusal traced to a missing command:

| Owner asked | Supervisor answered | Missing |
|---|---|---|
| Message project supervisors and task agents | no messaging command | `message.send`, `session.message.send`, `session.nudge` |
| Start the draft tasks | no state-change command | `task.activate` |
| Delete a draft and recreate it active | deletion unavailable; supersede rejected | `task.delete`, `task.cancel`, `task.complete` |
| (review blocked) | cannot recover the result or restart review | `task.retry`, `attempt.*`, `result.*`, `review.decision.*`, `decision.resolve` |
| (repeated) | "worker start hasn't been verified" | `dashboard.swarm`, `session.list`, `attempt.show`, `worker.list` |
| (02:36) | "capability name remains unverified" | no code to read at all |

Its own instructions also tell it to create drafts unless told otherwise, so
work it files does not run.

The owner's direction: functionality first, security later. Every supervisor,
global and project, gets everything the owner can do, plus read-only access to
every project's code. Supervisors also author playbooks (the Markdown) and
review the compiled output.

The work ships in two phases, by owner decision:

- **Phase 1, this design:** every owner command. It is contained in
  aq-server and fixes every refusal above except the last one.
- **Phase 2, its own design and plan:** read-only code access (see §4).

## Goals

1. Every registry command with an owner route is callable by every supervisor,
   with owner semantics, through the existing `aq-supervisor-host tool NAME`
   path. New owner commands become supervisor commands with no extra work.
2. A supervisor can register and edit playbook sources and read what they
   compiled to (sources, catalog definitions, loaded definitions, admissions,
   runs). Point 1 covers this; no playbook-specific work.
3. Supervisor instructions match the new reality: active tasks by default and
   owner powers granted.

Phase 2 goals (not this design): clone and fetch any project, including task
branches, read it with git and ripgrep, and never push.

## Non-goals

- Least privilege, per-command scopes for supervisors, or audit beyond what the
  command endpoint already records. Security comes later.
- Pushing code, merging PRs, or editing repositories. Implementation still goes
  to worker tasks.
- Host operations outside the AQ API (docker, systemd, deploys).
- Browser-only routes: dashboard assets and the interactive terminal
  WebSocket. `terminal.output`, `terminal.tail` and the other terminal reads
  are in, since they are ordinary owner GETs.
- Moving supervisor instructions into a playbook (worth doing under "policy
  lives in playbooks", but separate).

## Anti-goals

- A second per-command allowlist. Growing lists is how supervisors ended up
  with 21 of 154; the result must not need touching when an owner command is
  added.
- Handing supervisors an owner browser identity or a Cloudflare Access token.
  The per-turn supervisor credential and its fence stay the authority.
- Behavior that differs between a project supervisor and the global one.

## How it works today

- **Supervisor tools.** The agent runs `aq-supervisor-host tool NAME` with a
  `CommandRequest` on stdin (`crates/aq-supervisor-host/src/tool.rs`). That
  posts to `api/v1/commands/{name}` on the machine listener with the turn's
  `aqsv1_` bearer. `commands::invoke` (`crates/aq-server/src/commands.rs:181`)
  picks `spec.route(Principal::Supervisor)` and forwards in-process. With no
  supervisor route, the command is refused `NotAvailable`.
- **Supervisor routes.** These are set by hand. `catalog.rs:1284` maps 10
  reads: nine to `/v1/supervisor/read/...`, and `project.list` to
  `/v1/supervisor/projects`. `catalog.rs:129-173` covers `whoami`,
  `operation.show`, `task.create` and `task.graph.create`. `lib.rs:285` covers
  seven graph actions.
- **Owner bridge.** `manual_agents::supervisor_nests`
  (`crates/aq-server/src/manual_agents.rs:61`) nests owner routers behind
  supervisor gates on the machine listener:
  - `/v1/supervisor/read` wraps `agent_access::human::routes`. It accepts GET
    and HEAD only, takes scope `graph.read`, and a project supervisor is
    narrowed to its own project (`crates/aq-supervisor-control/src/read.rs`).
  - `/v1/supervisor/admin` wraps the settings, role, internal-work and
    playbook-authoring routers, with a path and method allowlist and scope
    `settings` (`crates/aq-supervisor-control/src/admin.rs`).

  Both gates authenticate the bearer, resolve the approving owner, and insert
  `ReadOwner` into request extensions. Owner handlers that resolve the user
  through `authz::resolve` (`crates/aq-server/src/authz.rs:20`) already treat
  that extension as the owner. There are two exceptions:
  - `manual_agents::service`'s owner check rebuilds the request from headers
    only (`manual_agents.rs:15-24`). So the `aq-manual-sessions` routes and
    `aq-supervisor-control/src/api.rs` (`supervisor.list`, `supervisor.set`)
    ignore the extension today.
  - The existing supervisor action routes for `task.create`,
    `task.graph.create` and the graph actions narrow a project supervisor to
    its own project (`agent_access/human/graph_actions.rs:734-737`).
- **Credentials.** A credential carries `SCOPES`: `graph.read`, `graph.write`,
  `enqueue`, `messaging`, `decisions`, `settings`
  (`crates/aq-supervisor-control/src/authority.rs:63`).
- **Owner router.** The owner API is assembled in `crates/aq-server/src/lib.rs`
  around lines 290-330 as one large `Router::new().merge(...)` chain on the
  admin listener, including the dashboard assets.
- **Git.** `/v1/git/{repository}/{*path}`
  (`crates/aq-server/src/profiled_execution/git_grant.rs:58`) proxies git
  smart-HTTP to aq-git's HTTP socket, for a node credential plus an
  `AQ-Attempt` grant. Pushes are limited to the grant's task ref.
  - The route is mounted only under the `delivery_playbook` feature (Epic 0,
    off by default).
  - It needs `[delivery] git_http_socket` on the server and
    `AQ_GIT_HTTP_SOCKET` on aq-git.
  - aq-git main requires grant headers (`aq-base`, `aq-task-ref`) even for
    upload-pack.
  - On ms1 today the route returns 404 and aq-git has no HTTP socket.
- **Supervisor container.** The supervisor runs as uid 10001 on a read-only
  root filesystem. The image is `debian:bookworm-slim` plus ca-certificates,
  tmux and libssl3, so it has no git and no ripgrep. Each supervisor has a
  writable `/workspace` volume that survives turns. It reaches the server at `http://server:8081`.
- **Instructions.** These are hard-coded in
  `crates/aq-supervisor-host/src/service/plan.rs:58`. They say "create draft
  tasks ... active only when the owner asks", "this workspace is not a project
  checkout", and "do not ... alter ... owner-only configuration".

## Approaches considered

Commands:

- **One owner mount for all owner routes (chosen).** Mount the whole owner API
  under one supervisor gate and generate a supervisor route for every owner
  command. It is generic, needs no per-handler work, and new commands come
  free.
- *Grow the allowlists.* Rejected: this is the status quo that produced 21 of
  154, and it is an anti-goal.
- *Give supervisors an owner login.* Rejected: it bypasses the supervisor
  credential and fence, and cannot work behind Cloudflare Access.

Code, for phase 2 (the owner picked A; the builder check showed it needs aq-git work and an ms1 rollout):

- **A. Read-only git through the server's `/v1/git` proxy (chosen).** The
  supervisor clones and fetches with its own bearer, and pushes are refused. It
  gets real checkouts and ordinary tools, and sees task branches, so it can
  review unmerged agent work.
- *B. Server commands* (`repo.tree`, `repo.show`, `repo.grep`) backed by
  aq-git. This means more new code and gives the agent weaker tools.
- *C. Read-only mounts of host checkouts.* These are stale, host-local, and
  have no task branches.

## Design

### 1. One owner mount: `/v1/supervisor/owner`

- Factor the owner API router out of `lib.rs` into one function, for example
  `owner_api(&state, ...) -> Router`. It holds everything the admin listener
  serves except dashboard static assets. The admin listener uses it unchanged.
- On the machine listener, nest it at `/v1/supervisor/owner` behind a new gate
  in `aq-supervisor-control`. The gate is the read gate without its method
  check and without project narrowing. It authenticates the bearer, holds the
  fence through the handler, and inserts `ReadOwner`. It accepts any method.
- Replace `/v1/supervisor/read` and `/v1/supervisor/admin` with the new mount.
  Move their callers (registry routes, and `aq-internal-work/src/notice.rs`
  `RUNS`) to `/v1/supervisor/owner/...`. Clean cut, no compatibility shim.
- The owner API's CSRF guard (`X-AQ-Request`) must not block the supervisor.
  The bearer is the authority, as the admin mount already documents. Either
  the gate sets the header on the inner request, or the CSRF layer is applied
  outside the shared router on the admin listener only.

### 2. Every owner command gets a supervisor route

- In `aq-command-registry`, set the supervisor route generically:
  - For each command whose owner route is an HTTP route, the supervisor route
    is the same method and path under `/v1/supervisor/owner`.
  - Commands that already have a dedicated supervisor route keep it (`whoami`
    local, `task.create` and the graph actions through `Route::action()`).
  - Delete the hand list at `catalog.rs:1284` and the graph-action list at
    `lib.rs:285` wherever the generic rule covers them.
- The registry consistency tests (`crates/aq-command-registry/src/tests.rs`)
  change to assert: every command with an owner route has a supervisor route.
- `aq-supervisor-host tool commands` lists from the registry, so the tool
  surface grows with no host change.

### 3. Credential scope

- Add one scope, `owner`, to `SCOPES`. The new gate requires it. Existing
  scopes stay for the routes that still check them (actions, replies,
  enqueue).

### 4. Read-only code (second phase, separate design)

Read-only code access ships second, under its own design and plan.

- The owner picked approach A below.
- It needs a read-only fetch mode in aq-git that requires no grant.
- It needs the server's `/v1/git` proxy taken out of `delivery_playbook` for supervisor callers.
- It needs an ms1 config and aq-git rollout (the HTTP socket).
- It needs git and ripgrep in the supervisor image, plus a git credential helper.
- Pushes stay refused.

This document's code paragraphs record the direction, not this phase's work.

### 5. Instructions

Rewrite the instruction text in `service/plan.rs`:

- All supervisors may coordinate every project. Remove "the server restricts
  your commands to that project" while keeping the project supervisor's focus
  on its own project as guidance, not a restriction.
- Create tasks **active** unless the owner asks for a draft. Use
  `task.activate` for drafts the owner wants started.
- Owner commands are available. Remove "do not alter owner-only
  configuration". Keep "do not deploy or alter host services or credentials",
  since those are outside the API anyway.
- Code: keep "this workspace is not a project checkout; delegate
  implementation to worker tasks" until phase 2 adds reading.
- Playbooks: author with `playbook.source.register` and
  `playbook.source.edit`, then review the result with `playbook.source.show`
  and `playbook.catalog.definitions` before activating.

### 6. Flag and rollout

- This lands on main behind a compile-time feature, `supervisor_full_access`,
  on `aq-server`, `aq-command-registry` and `aq-supervisor-control`, per the
  epic rule. Off: today's behavior. The flag is removed once the owner
  confirms the epic.
- Roll the server and the supervisor image together. The supervisor binary
  links `aq-server` and the registry, and a mismatched pair broke the fleet on
  2026-11-19.

### 7. Calls made after the builder check (author's calls; the owner can override)

- **No project narrowing anywhere.** Remove the project check from the
  supervisor action routes (`graph_actions.rs:734-737`), and drop the project
  narrowing in the new gate. `task.create`, `task.graph.create` and the graph
  actions keep their dedicated routes, so task-creation receipts still reach
  the reply through `AQ_SUPERVISOR_ACTIONS_FILE`.
- **`operation.show` for a supervisor** looks up the supervisor's own log
  first, then the owner's operations. That way an uncertain `task.activate`
  sent through the owner mount can still be checked.
- **Owner check without the extension.** `manual_agents::service`'s owner
  check also accepts the `ReadOwner` extension, as `authz::resolve` does. Then
  manual sessions, `supervisor.list` and `supervisor.set` work under the mount.
- **Locks.** The new gate authenticates and commits before calling the
  handler, as `commands::caller` does. It does not hold the
  session-head/credential fence through the handler. This lets a supervisor
  act on its own session without blocking itself, and keeps its calls
  concurrent. The credential's expiry and revocation are still checked on
  every call.
- **Late playbook engine.** When the playbook engine opens late
  (`open_later`), the machine listener's owner mount is rebuilt the same way
  the admin router is. Playbook, review and media commands then stay
  available to supervisors.
- **Query parameters.** `aq_command_registry::queries::declared` also strips
  the `/v1/supervisor/owner` prefix, as it does for `/v1/supervisor/read`
  today.
- **Flag coverage.** `supervisor_full_access` spans `aq-server`,
  `aq-command-registry`, `aq-supervisor-control`, `aq-supervisor-host` (the
  instructions) and `aq-internal-work` (the `RUNS` path). It is passed to the
  image builds through a `CARGO_FEATURES` build-arg in `Dockerfile` and
  `Dockerfile.supervisor-host`. ms1 builds with it on. CI keeps building
  without it, plus one feature-on test run.

### 8. Review gates (owner decision)

- Supervisors may resolve review decisions, including human-gated ones. The
  owner may delegate a review to a supervisor, so nothing enforces the gate in
  code.
- The instructions say: respect human review gates unless the owner has
  delegated that review to you.
- Supervisor actions are recorded as the owner, with no "via supervisor"
  marker. Telling the two apart is left to the later security work.

## Acceptance

- On the ms1 stack, `aq-supervisor-host tool commands` in the global
  supervisor lists every command that has an owner route.
- In a supervisor turn: `task.activate` on a draft succeeds, `task.delete`
  succeeds, `message.send` to a task mailbox succeeds, `dashboard.swarm` and
  `session.list` return data, `playbook.source.register` starts authoring, and
  `playbook.catalog.definitions` shows the compiled output.
- A project supervisor can read and change another project.
- The six project supervisors and the global one restart on the new image and
  report idle.
- Built with the flag off, the existing supervisor tests pass unchanged.

## Left to the builder

- The exact name and signature of the factored owner router, and how to keep
  dashboard assets out of it.
- How CSRF is bypassed: header injection versus layer placement.
- Which commands lack an HTTP owner route and are therefore skipped (CLI-local
  ones such as `whoami`).
- Test placement and fixtures.
