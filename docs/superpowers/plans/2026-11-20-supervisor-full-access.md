# Supervisor full access (phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every supervisor can call every owner command, with owner semantics, behind the compile-time feature `supervisor_full_access`.

**Architecture:** A new machine-listener route `/v1/supervisor/owner/{*rest}` authenticates the supervisor bearer (scope `owner`). It then hands the request, with the prefix stripped, to the live admin app in-process, running as the approving owner. Owner identity reaches handlers two ways: through the existing `ReadOwner` request extension, and through a new task-local, which covers handlers that rebuild requests from headers. The command registry gives every owner-routed command a supervisor route under that prefix. The old `/v1/supervisor/read` and `/v1/supervisor/admin` nests are compiled out when the flag is on.

**Tech Stack:** Rust, axum 0.8, tower `ServiceExt::oneshot`, tokio `task_local!`, sqlx/PostgreSQL, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-11-20-supervisor-full-access-design.md`

## Global Constraints

- Feature name `supervisor_full_access`. Flag off means today's behavior, byte for byte: every existing test passes unchanged without the feature.
- Mount prefix `/v1/supervisor/owner`. New credential scope `owner`.
- The admin listener keeps `authz::csrf_guard` for browser requests. Only requests that came through the supervisor mount carry `x-aq-request: 1`, which the bridge injects.
- No project narrowing for supervisors anywhere when the flag is on.
- No new test binaries (`build-budget.toml`). New tests go in `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs`, declared as `mod owner;` in `supervisor_turns.rs` the way `mod admin;` is. Registry tests go in `crates/aq-command-registry/src/tests.rs`.
- Every `cargo test` or `cargo nextest` runs under `timeout 900`. Build in this worktree's own `./target`.
- Images build with `CARGO_PROFILE=ci-image` on ms1.

## Review Focus

1. An expired or revoked supervisor credential on the mount returns 401 and never falls through to an owner identity. Covered by a test in Task 2.
2. A browser request to the admin listener without `x-aq-request` is still refused, since the bridge must not weaken CSRF there. Covered by a test in Task 2.
3. A request to `/v1/supervisor/owner/...` with only `x-aq-dev-email` (dev owner) and no bearer is refused with 401. Covered by a test in Task 2.
4. Query strings survive the bridge, so `task.list` with `limit` pages correctly through the command endpoint. Covered by a test in Task 3.
5. The supervisor's bearer is not forwarded to owner handlers: the `authorization` header is removed before the hand-off. Covered by a test in Task 2.

Not covered by tests: the late playbook engine (`open_later`). The bridge forwards to the same `Router` value that `open_later` returns and swaps, so it follows the swap. Check this by reading the code in Task 2's review.

---

### Task 1: Feature flag, `owner` scope, image and CI wiring

**Files:**
- Modify: `crates/aq-server/Cargo.toml` ([features])
- Modify: `crates/aq-command-registry/Cargo.toml`, `crates/aq-supervisor-control/Cargo.toml`, `crates/aq-internal-work/Cargo.toml`, `crates/aq-supervisor-host/Cargo.toml` (add `[features] supervisor_full_access = [...]`)
- Modify: `crates/aq-supervisor-control/src/authority.rs:62` (`SCOPES`)
- Modify: `Dockerfile:34-53`, `Dockerfile.supervisor-host:21`
- Modify: `.github/workflows/ci.yml` (the `tests (feature-on runs)` job, around line 196-235)
- Test: `crates/aq-supervisor-control/src/authority.rs` (unit test module, create it if none exists)

**Interfaces:**
- Produces: the feature `supervisor_full_access` on all five crates. `aq-server`'s feature enables `aq-command-registry/supervisor_full_access`, `aq-supervisor-control/supervisor_full_access` and `aq-internal-work/supervisor_full_access`. `aq-supervisor-host`'s feature enables `aq-server/supervisor_full_access`.
- Produces: when the flag is on, `SCOPES` has 7 entries ending in `"owner"`. When it is off, it is unchanged at 6.

- [ ] **Step 1: Write the failing test** `scopes_include_owner_when_flagged` (`#[cfg(feature = "supervisor_full_access")]`): `assert!(SCOPES.contains(&"owner")); assert_eq!(SCOPES.len(), 7);`
- [ ] **Step 2: Run it to make sure it fails.** Run `timeout 900 cargo test -p aq-supervisor-control --features supervisor_full_access scopes_include_owner`. Expected: compile error (unknown feature) or FAIL.
- [ ] **Step 3: Add the features and make `SCOPES` cfg-dependent.** Use two `#[cfg]` definitions of `pub const SCOPES`, `[&str; 6]` and `[&str; 7]`. Callers use `SCOPES.iter()`, so the length change is safe.
- [ ] **Step 4: Run the test** (expect PASS). Then confirm the flag-off build still compiles: `timeout 900 cargo check -p aq-server -p aq-supervisor-host` and `timeout 900 cargo check -p aq-server -p aq-supervisor-host --features aq-supervisor-host/supervisor_full_access`.
- [ ] **Step 5: Image build-arg.** Add `ARG CARGO_FEATURES=""` to both Dockerfiles. Pass `${CARGO_FEATURES:+--features $CARGO_FEATURES}` to the `cargo build` of `aq-server` (`Dockerfile`) and of `aq-supervisor-host` (`Dockerfile.supervisor-host`). Check with `docker build --build-arg CARGO_PROFILE=ci-image --build-arg CARGO_FEATURES=supervisor_full_access -f Dockerfile.supervisor-host .`; it must build.
- [ ] **Step 6: CI.** Add two steps to the `tests (feature-on runs)` job:
  - `cargo nextest run -p aq-command-registry --locked --profile ci --features supervisor_full_access`
  - `cargo nextest run -p aq-server --locked --no-fail-fast --profile ci --features supervisor_full_access --test it_dashboard -E 'test(/^supervisor_turns::/)'`
- [ ] **Step 7: Commit** with message `feat(supervisor): supervisor_full_access flag, owner scope, image and CI wiring`.

### Task 2: The owner bridge

**Files:**
- Create: `crates/aq-supervisor-control/src/owner.rs` (under `#[cfg(feature = "supervisor_full_access")]` in `lib.rs`)
- Modify: `crates/aq-supervisor-control/Cargo.toml` (add `tower` with `util` if it is not already a dependency, and `tokio` with `rt` for `task_local!`)
- Modify: `crates/aq-server/src/authz.rs:19-31` (`resolve`)
- Modify: `crates/aq-server/src/manual_agents.rs:61-71` (`supervisor_nests`)
- Modify: `crates/aq-server/src/lib.rs` (`build_worker_router_inner` and `serve` near the `let worker_app = build_worker_router(state);` line)
- Test: `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs` (new; `#![cfg(feature = "supervisor_full_access")]`, with `mod owner;` added in `supervisor_turns.rs`)
- Modify: `crates/aq-server/tests/it_dashboard/supervisor_turns/admin.rs` and the read-nest assertions in `supervisor_turns.rs` (cfg them `not(feature = "supervisor_full_access")`)

**Interfaces:**
- Produces in `aq_supervisor_control::owner`:
  - `tokio::task_local! { pub static DELEGATED_OWNER: crate::read::ReadOwner; }`
  - `pub const PREFIX: &str = "/v1/supervisor/owner";`
  - `pub fn routes<S: Clone + Send + Sync + 'static>(owner_app: axum::Router, pool: sqlx::PgPool, clock: crate::read::Clock) -> axum::Router<S>`
- Produces in `aq_server`: `pub fn build_worker_router_with_owner_app(state: AppState, owner_app: axum::Router) -> axum::Router`. The existing `build_worker_router(state)` keeps its signature and mounts no bridge.
- Consumes: `crate::read::bearer(pool, clock, scope)` (its scope parameter becomes `&'static str`, already the case), `crate::read::Permit`, `crate::read::ReadOwner`.

Bridge handler behavior, for any method on `{PREFIX}/{*rest}`:
1. `bearer(pool, clock, "owner")` checks the request headers. On error, return that error (401 or 403).
2. Drop `permit.guard`, so the locks are released before the owner handler runs.
3. Rebuild the request URI as `/{rest}` plus the original query.
4. Remove `authorization`, `cookie` and `x-aq-dev-email`, then set `x-aq-request: 1`.
5. Insert `permit.owner` as an extension.
6. Return `DELEGATED_OWNER.scope(permit.owner.clone(), owner_app.oneshot(req)).await`.

`authz::resolve` gains, after the `ReadOwner` extension check and under the flag, a check for `aq_supervisor_control::owner::DELEGATED_OWNER.try_with(Clone::clone)`, which builds the same `User`. With the flag on, `supervisor_nests(&state, owner_app)` returns `/v1/supervisor/projects` plus `owner::routes(...)`, and no read or admin nests.

- [ ] **Step 1: Write the failing tests** in `owner.rs`. The fixture builds a running global supervisor with a credential the same way `admin.rs::admin()` does, but builds `machine = aq_server::build_worker_router_with_owner_app(f.state.clone(), aq_server::build_admin_router(f.state.clone()))`. Tests:
  - `supervisor_activates_a_draft_through_the_owner_mount`: create a draft task as the owner, POST `/v1/supervisor/owner/api/v1/actions` with a `task.activate` action as the supervisor, and expect 200 with the task state active on re-read.
  - `supervisor_reads_swarm_and_supervisor_list_through_the_owner_mount`: GET `/v1/supervisor/owner/api/...` for `dashboard.swarm`'s path and for `supervisor.list`'s path (take both paths from the registry: `registry().get(name).owner.path`). Both return 200; `supervisor.list` proves the task-local reaches `manual_agents`/`supervisor_control` handlers.
  - `owner_mount_refuses_without_bearer_even_with_dev_owner_header` (Review Focus 3): send only `x-aq-dev-email: owner@localhost` and expect 401.
  - `owner_mount_refuses_expired_and_revoked_credentials` (Review Focus 1): set `revoked_at_ms`, and separately advance the clock past `expires_at_ms`. Expect 401 each time.
  - `owner_mount_refuses_a_credential_without_owner_scope`: reissue with the six old scopes (same technique as `reissue_with_five_scopes`) and expect 403.
  - `admin_listener_still_requires_csrf` (Review Focus 2): a POST to `/api/v1/actions` on the admin router with the dev owner header and no `x-aq-request` is refused exactly as on main.
  - `owner_mount_does_not_forward_the_bearer` (Review Focus 5): owner app is a test router that echoes whether `authorization` is present. Expect absent and `x-aq-request: 1` present.
- [ ] **Step 2: Run** `timeout 900 cargo nextest run -p aq-server --features supervisor_full_access --test it_dashboard -E 'test(/^supervisor_turns::owner::/)'`. Expected: FAIL (`build_worker_router_with_owner_app` is not defined).
- [ ] **Step 3: Implement** `owner.rs`, the `resolve` task-local check, `supervisor_nests(&state, owner_app: Option<axum::Router>)`, `build_worker_router_with_owner_app`, and in `serve` (flag on) `build_worker_router_with_owner_app(state, admin_app.clone())`. Gate the old read and admin nest tests with `#[cfg(not(feature = "supervisor_full_access"))]`.
- [ ] **Step 4: Run the owner tests** (expect PASS). Then run `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^supervisor_turns::/)'` without the feature (expect PASS, unchanged).
- [ ] **Step 5: Commit** with message `feat(supervisor): /v1/supervisor/owner bridge to the owner app`.

### Task 3: Every owner command gets a supervisor route

**Files:**
- Modify: `crates/aq-command-registry/src/lib.rs:255-300` (`Registry::new`)
- Modify: `crates/aq-command-registry/src/catalog.rs:1283-1307` (hand map of supervisor reads)
- Modify: `crates/aq-command-registry/src/queries.rs:100-101` (`declared`)
- Test: `crates/aq-command-registry/src/tests.rs`; `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs`

**Interfaces:**
- Consumes: `aq_supervisor_control::owner::PREFIX`. Duplicate the literal `"/v1/supervisor/owner"` as `pub const OWNER_PREFIX` in `aq-command-registry` instead of adding a dependency, and assert in a test that the two are equal.
- Produces: with the flag on, every `CommandSpec` with `owner: Some(r)` where `r.method != "LOCAL"` has `supervisor: Some(_)`. Commands whose supervisor route the registry already sets keep their existing route: `whoami`, `operation.show`, `task.create`, `task.graph.create`, `task.edit`, `task.reprioritize`, `task.role.set`, `task.route.set`, `task.route.clear`, `task.dependency.add`, `task.dependency.remove`. Every other command gets `Route { path: <OWNER_PREFIX + owner.path, leaked once>, ..owner }`.

- [ ] **Step 1: Write the failing tests** (all `#[cfg(feature = "supervisor_full_access")]`):
  - `every_owner_command_has_a_supervisor_route`: every command with an HTTP owner route has `supervisor.is_some()`. Commands with a supervisor route set elsewhere keep it. Every new route's path equals `format!("{OWNER_PREFIX}{}", owner.path)` and keeps the owner's method, body and params.
  - `owner_prefix_matches_the_bridge`, in `owner.rs` (aq-server depends on both crates, which avoids a dependency cycle): `aq_command_registry::OWNER_PREFIX == aq_supervisor_control::owner::PREFIX`.
  - Existing `supervisor_reads_declare_the_owner_query` must pass with the new prefix. It already compares `supervisor.path.ends_with(owner.path)`.
  - In `owner.rs`: `supervisor_invokes_owner_commands_through_the_command_endpoint`:
    - POST `/api/v1/commands/task.activate` on the machine router with the bearer and a `CommandRequest`, expecting success.
    - GET `/api/v1/commands` as the supervisor, expecting it to list `task.delete`, `message.send` and `playbook.catalog.definitions`.
  - In `owner.rs`: `task_list_limit_survives_the_bridge` (Review Focus 4): create 3 tasks, invoke `task.list` with `{"project_id":p,"limit":2}` through the command endpoint, and expect exactly 2 items plus a cursor.
- [ ] **Step 2: Run** `timeout 900 cargo nextest run -p aq-command-registry --features supervisor_full_access` and the owner module. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - With the flag on, skip the hand map loop in `catalog.rs`.
  - At the end of `Registry::new`, before sorting, fill missing supervisor routes from owner routes. Use `Box::leak(format!(..).into_boxed_str())`; the registry is built once per process (tests build it a few times, a bounded leak).
  - `queries::declared` also strips `OWNER_PREFIX`.
- [ ] **Step 4: Run** both feature-on suites (expect PASS), then `timeout 900 cargo nextest run -p aq-command-registry` without the feature (expect PASS, unchanged).
- [ ] **Step 5: Commit** with message `feat(registry): every owner command routes for supervisors through the owner mount`.

### Task 4: No project narrowing, and `operation.show` falls back to the owner

**Files:**
- Modify: `crates/aq-server/src/agent_access/human/graph_actions.rs:733-737`
- Modify: `crates/aq-server/src/agent_access/human/enqueue/supervisor.rs:232-245` (`lookup`)
- Test: `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs`; the cross-project assertion in `supervisor_turns.rs` (`project_supervisor_mail_history_and_reads_remain_scoped...`)

**Interfaces:**
- Consumes: `aq_supervisor_control::read::owner(&mut tx, actor) -> Result<ReadOwner, Error>`. Use the same owner-journal lookup `dashboard_api::actions::get_operation` uses (`journal::lookup_for_authorization(&pool, owner.id, id)` plus the response builder), keyed by the approving owner's id.

- [ ] **Step 1: Write the failing tests:**
  - `project_supervisor_creates_a_task_in_another_project`: a project supervisor for A runs `task.create` (its dedicated action route) in project B, and gets 200.
  - `operation_show_finds_an_operation_sent_through_the_owner_mount`: the supervisor sends `task.activate` through the command endpoint with operation id X, then `operation.show` X as the supervisor returns that operation.
  - `operation_show_still_finds_the_supervisors_own_task_create`: a supervisor `task.create` with op Y, then `operation.show` Y succeeds. This guards the existing behavior.
  - Change the cross-project read in the scoped test: under the flag it expects 200 through `/v1/supervisor/owner/...`; without the flag it keeps expecting 403.
- [ ] **Step 2: Run the owner module with the feature.** Expected: FAIL on the first two tests.
- [ ] **Step 3: Implement.** With the flag on, drop `&& p.project_id.is_none_or(|id| id == project)` (keep the session and `enqueue` checks). In `lookup`, when `lookup_scoped` returns not-found, resolve the approving owner and look the id up in the owner's journal.
- [ ] **Step 4: Run** the owner module with the feature, and the whole `supervisor_turns::` filter with and without the feature. Expected: PASS.
- [ ] **Step 5: Commit** with message `feat(supervisor): no project narrowing; operation.show falls back to the owner journal`.

### Task 5: Instructions and the internal-work notice path

**Files:**
- Modify: `crates/aq-supervisor-host/src/service/plan.rs:50-62`
- Modify: `crates/aq-internal-work/src/notice.rs:11` (`RUNS`)
- Test: `crates/aq-supervisor-host/src/service/plan.rs` tests (or the crate's existing plan tests); `crates/aq-internal-work` notice tests

**Interfaces:**
- Produces: when the flag is on, `RUNS = "/v1/supervisor/owner/api/internal-work/runs"`; when off, it is unchanged.

Instruction text when the flag is on, replacing both scope variants and the body.

The project scope line is: `You are the supervisor for project {label} ({project}); focus there, but you may read and change any project.`

The global scope line keeps its current text.

The body, as exact copy:

```
You manage AQ work through authenticated tools and hold the owner's powers: every owner command is available to you.
Use /usr/local/bin/aq-supervisor-host tool commands to discover commands, or tool commands NAME for one command's schema. To invoke one, run /usr/local/bin/aq-supervisor-host tool NAME with one JSON CommandRequest on stdin, e.g. {"params":{"project_id":"UUID"}} for task.list. Use project.list for project identifiers and revisions; task.list and task.show expose current task versions. Mutations also need a fresh operation_id, a subject and the current expected versions; inspect the command schema and current project/task before writing. Retain the same operation_id and exact body when checking an uncertain request; never invent success or replay a replacement.
Create tasks active unless the owner asks for a draft; use task.activate to start a draft the owner wants run. Include requirements, acceptance criteria and dependencies. Distinguish queued work, active attempts, retained results and actual completion; verify with dashboard.swarm, session.list and attempt.show rather than assuming a worker started.
This private central workspace is not a project checkout: delegate implementation to worker tasks instead of editing files here.
Playbooks: write the Markdown with playbook.source.register or playbook.source.edit, review what it compiled to with playbook.source.show and playbook.catalog.definitions, and only then activate it.
Workers should report completion/blockers to supervisor conversation {conversation} through their AQ message tool. Respond when these messages arrive and inspect actual state. Do not claim you will monitor work without an admitted wait or future event.
Respect human review gates unless the owner has delegated that review to you.
Your final answer is published automatically to this conversation with any confirmed task creation receipts. You may return ordinary concise text; do not print credentials or read authentication files. AQ credential paths are for the tool implementation only. Do not deploy or alter host services or credentials.
```

- [ ] **Step 1: Write the failing tests** (flag on):
  - `flagged_instructions_grant_owner_powers_and_default_active`: the instructions string contains `"Create tasks active unless the owner asks for a draft"`, contains `"every owner command is available to you"`, and does not contain `"Create draft tasks for proposals"` or `"owner-only configuration"`.
  - The project-scope variant contains `"you may read and change any project"`.
  - `notice_runs_path_uses_the_owner_mount`: `RUNS` starts with `/v1/supervisor/owner/`.
- [ ] **Step 2: Run** `timeout 900 cargo test -p aq-supervisor-host --features supervisor_full_access plan` and `timeout 900 cargo test -p aq-internal-work --features supervisor_full_access notice`. Expected: FAIL.
- [ ] **Step 3: Implement** the cfg'd text and `RUNS`. If a test pins the instruction digest (for example in `tests/`), add the flagged digest beside the unflagged one.
- [ ] **Step 4: Run** both crates' tests with and without the feature. Expected: PASS.
- [ ] **Step 5: Commit** with message `feat(supervisor): instructions grant owner powers and default tasks to active`.

### Task 6: Roll out to ms1 and verify live

This task is operational; there is no TDD cycle. Follow the 2026-11-20 rollout record in `~/.local/share/aq-ms1/supervisor-rollout-20261120/`.

- [ ] **Step 1: Run the clippy and full feature-on runs.**
  - `timeout 900 cargo clippy -p aq-server -p aq-supervisor-host --features aq-supervisor-host/supervisor_full_access --all-targets -- -D warnings`
  - The two CI feature-on commands from Task 1.
  - `python3 scripts/build-budget.py`
  - All must pass.
- [ ] **Step 2: Build the server image** from this branch with `--build-arg CARGO_PROFILE=ci-image --build-arg CARGO_FEATURES=supervisor_full_access`, tag `agent-q-server:ms1-sfa-<shortsha>`. Recreate the server with the rollout's `compose.sh up -d --no-deps --no-build --force-recreate --wait --wait-timeout 300 server`. Expect `curl -fsS http://127.0.0.1:28080/healthz` to print `{"status":"ok"}`.
- [ ] **Step 3: Build the supervisor binary.** Run `cargo build --locked --profile ci-image -p aq-supervisor-host --features supervisor_full_access` in `rust@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663`, with a user-owned `CARGO_HOME`. Overlay it on `agent-q-supervisor:ms1-b075860` as a new tag, and run the adapted `upgrade-supervisors.py` (previous image `sha256:5b7f12104d0292721f88d5366766e145b6cca418976a8cca774f61ae25588f8d`). Expect `Updated 7 supervisors`.
- [ ] **Step 4: Verify in a live global supervisor turn.** Send it one message asking it to:
  - list its commands and say how many it has (expect more than 140);
  - activate one draft task the owner names;
  - read `dashboard.swarm`;
  - draft a trivial playbook source and report its compiled definition.

  Check each effect through the owner CLI (`aq --local --origin http://127.0.0.1:28080 tasks show ...`).
- [ ] **Step 5: Verify the project supervisors.** Check that all 7 report idle or working in `supervisor_service_reports`, and that the global supervisor's `/state/host/service-error.txt` is not rewritten after the rollout.
- [ ] **Step 6: Record the rollout** in the rollout directory's notes. Open a PR from `supervisor-full-access` to main; the owner merges.
