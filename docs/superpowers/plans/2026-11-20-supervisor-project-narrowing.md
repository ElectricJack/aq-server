# Project supervisor narrowing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Behind `supervisor_full_access`, a project supervisor holds owner authority over its own project only, while the global supervisor keeps owner authority everywhere.

**Architecture:** The owner bridge in `aq-supervisor-control/src/owner.rs` adds one deny-by-default check for project supervisors: a request is forwarded only if every project it names is the supervisor's own, and it names at least one. The pre-#220 narrowing comes back for project supervisors on the dedicated routes (`may_enqueue`, `/v1/supervisor/projects`, the `operation.show` owner fallback). The project-scope instruction line changes. All new logic lives outside `crates/aq-server/src`.

**Tech Stack:** Rust, axum 0.8 (`axum::body::to_bytes`), serde_json, uuid, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-11-20-supervisor-project-narrowing-design.md` (and, for context, `docs/superpowers/specs/2026-11-20-supervisor-full-access-design.md`)

## Global Constraints

- Everything is under the existing feature `supervisor_full_access`. With the flag off, behavior is byte for byte unchanged and every existing test passes without the feature.
- The global supervisor (`permit.project == None`, `SupervisorPrincipal::project_id == None`) is unchanged everywhere.
- The refusal is HTTP 403 with message `Project supervisor can act only on its own project`.
- The action body bound is 65536 bytes, the same as the command endpoint's body bound.
- Zero net lines in `crates/aq-server/src`. `python3 scripts/build-budget.py` must print OK; the branch has 0 lines of headroom.
- No new test binaries. Integration tests go in `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs` (already `#![cfg(feature = "supervisor_full_access")]`) and `supervisor_turns.rs`. Unit tests go in `aq-supervisor-control`.
- Run every cargo test under `timeout 900`. Run `eval "$(scripts/test-db)"` in the same shell before integration tests.
- The commit trailer is `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. A path naming the own project and another one, e.g. `/api/x/projects/<own>/y/projects/<other>`, is refused: every `projects/<uuid>` pair must be the own project. Test in Task 1.
2. A `task.create` with `subject` `{type: project, id: own}` but `payload.project_id == other` is refused: all named projects must match. Test in Task 1.
3. A malformed or oversize action body from a project supervisor gets 400 or 413 and is not forwarded. Test in Task 1.
4. A non-UUID or differently cased UUID segment after `projects`: compare parsed `Uuid` values, so case differences match and non-UUIDs don't count as naming a project. Test in Task 1.
5. A GET that carries `project_id` only in its query (e.g. `task.overview?project_id=`) is refused, because only the path and action bodies count. Test in Task 1.

---

### Task 1: The bridge refuses project supervisors outside their project

**Files:**
- Modify: `crates/aq-supervisor-control/src/owner.rs` (the `routes` bridge closure; add a pure check)
- Test: `crates/aq-supervisor-control/src/owner.rs` (`#[cfg(test)] mod tests`), `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs`

**Interfaces:**
- Produces: `pub fn names_only(project: uuid::Uuid, method: &axum::http::Method, path: &str, body: Option<&serde_json::Value>) -> bool` in `aq_supervisor_control::owner`. It returns true iff the request names at least one project and every project it names equals `project`. The named projects are:
  - every path segment following a `projects` segment that parses as a `Uuid`;
  - for `POST /api/v1/actions`, also `body.payload.project_id` when present, and `body.subject.id` when `body.subject.type == "project"`.
  - `path` excludes the query string.
- Produces, for Task 1's tests: `async fn project_owner() -> Owner`. It is a fixture in `owner.rs` like `owner()`, but its supervisor is a real project supervisor of the bound fixture project. It also binds a second project and exposes it as `other: Uuid`.
  - Create the project supervisor the way `project_supervisor_mail_history_and_reads_remain_scoped_and_stop_revokes_tools` in `supervisor_turns.rs` does: inserting a `project_execution_bindings` row makes the trigger create its supervisor conversation. Use no DDL.
  - Replace the trigger-disabling, FK-dropping setup in `project_supervisor_creates_a_task_in_another_project` with this fixture.

- [ ] **Step 1: Write the failing unit tests** in `owner.rs` `mod tests`, using `p = Uuid::from_u128(1)`, `q = Uuid::from_u128(2)`:
  - `names_only_accepts_the_own_project_path`: `names_only(p, &GET, &format!("/api/human/projects/{p}/tasks"), None)` is true.
  - `names_only_refuses_another_or_no_project`: `/api/human/projects/{q}/tasks` gives false. `/api/v1/swarm` gives false. `/api/x/projects/{p}/y/projects/{q}` gives false (Review Focus 1). `/api/human/projects/not-a-uuid/tasks` gives false (Review Focus 4).
  - `names_only_matches_case_insensitively`: a path with `p` uppercased gives true (Review Focus 4).
  - `names_only_reads_action_bodies`: on `POST /api/v1/actions`, `{"subject":{"type":"task","id":"…"},"payload":{"project_id":p}}` gives true. The same with `q` gives false. `{"subject":{"type":"project","id":p},"payload":{}}` gives true. `{"subject":{"type":"project","id":p},"payload":{"project_id":q}}` gives false (Review Focus 2). `{"subject":{"type":"task","id":"…"},"payload":{}}` gives false.
  - `names_only_ignores_queries`: the path `/api/v1/tasks/overview` with no body gives false, and the caller never passes the query (Review Focus 5).
- [ ] **Step 2: Run** `timeout 900 cargo test -p aq-supervisor-control --features supervisor_full_access names_only`. Expected: FAIL (`names_only` is not defined).
- [ ] **Step 3: Implement `names_only` and the bridge check.** In the bridge closure, after the bearer check:
  - If `permit.project` is `Some(p)` and the request is `POST /api/v1/actions`, read the body with `axum::body::to_bytes(body, 65536)`. A body over the limit returns 413; JSON that fails to parse returns 400. Rebuild the forwarded body from those same bytes.
  - Call `names_only(p, method, path, body_json.as_ref())`. If it returns false, return `aq_manual_sessions::Error(StatusCode::FORBIDDEN, "Project supervisor can act only on its own project")` before forwarding.
  - The global supervisor (`None`) path is unchanged and never buffers its body.
- [ ] **Step 4: Run the unit tests.** Expected: PASS.
- [ ] **Step 5: Write the integration tests** in `owner.rs` (the flag is on throughout), using `project_owner()`:
  - `project_supervisor_activates_and_comments_in_its_own_project`:
    - `task.activate` on a draft in its own project through the command endpoint gives 200 and the task is active.
    - `task.comment.add` on that task gives 200.
    - `task.list` for its project gives 200.
  - `project_supervisor_is_refused_outside_its_project`: each of these gives 403 with the message above:
    - `task.activate` whose `project_id` is `other`;
    - `task.list` with `project_id` `other`;
    - `dashboard.swarm`, `worker.list` and `playbook.source.list`.
  - `project_supervisor_action_body_bounds` (Review Focus 3): a direct `POST /v1/supervisor/owner/api/v1/actions` whose body is invalid JSON gives 400, and a 70000-byte body gives 413.
  - The global supervisor's existing tests pass unchanged.
- [ ] **Step 6: Run** with the test DB: `timeout 900 cargo nextest run -p aq-server --locked --no-fail-fast --features supervisor_full_access --test it_dashboard -E 'test(/^supervisor_turns::/)'`. Expected: PASS. (`project_supervisor_creates_a_task_in_another_project` is handled in Task 2.) Also run without the feature (expect PASS), then `python3 scripts/build-budget.py` (expect OK).
- [ ] **Step 7: Commit** with message `feat(supervisor): project supervisors reach the owner bridge only for their own project`.

### Task 2: Restore project narrowing on the dedicated routes and instructions

**Files:**
- Modify: `crates/aq-supervisor-control/src/authority.rs:97-102` (`may_enqueue`)
- Modify: `crates/aq-supervisor-control/src/read.rs:~138-141` (`/v1/supervisor/projects`)
- Modify: `crates/aq-supervisor-control/src/owner.rs` (`approving_owner`)
- Modify: `crates/aq-supervisor-host/src/service/plan.rs:~48` (flag-on project scope line) and its test at ~376
- Test: `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs`, `crates/aq-server/tests/it_dashboard/supervisor_turns.rs` (the scoped test's flag-on branch)

**Interfaces:**
- Consumes: `project_owner()` from Task 1.
- Produces:
  - `may_enqueue` is `user == self.session_id && scopes contain "enqueue" && self.project_id.is_none_or(|id| id == project)` in both flag states. Drop the `cfg!` term.
  - `/v1/supervisor/projects` binds `permit.project` in both flag states.
  - `approving_owner` returns `None` when `p.project_id.is_some()`, so the owner-journal fallback is for the global supervisor only.
  - The flag-on project scope line is exact copy: `You supervise only project {label} ({project}); you hold the owner's powers inside it. The server refuses other projects and global administration.`

- [ ] **Step 1: Write and adjust the failing tests:**
  - Rename `project_supervisor_creates_a_task_in_another_project` to `project_supervisor_cannot_create_a_task_in_another_project`, using `project_owner()`. Assert the cross-project `task.create` on the dedicated action route is refused with the same status the flag-off build returns for it; check flag-off behavior to get the code. Also assert a create in its own project gives 200.
  - In `supervisor_turns.rs`'s scoped test, make the flag-on branch expect only the supervisor's own project from `project.list`, the same as flag off. Collapse the cfg split if both branches become identical.
  - `project_supervisor_operation_show_has_no_owner_fallback`: a project supervisor sends `task.activate` in its own project through the bridge with operation id X. `operation.show` X as that supervisor gives 404. The global supervisor's `operation_show_finds_an_operation_sent_through_the_owner_mount` still passes.
  - In `plan.rs`, change the scoped assertion to `contains("you hold the owner's powers inside it")` and `!contains("you may read and change any project")`.
- [ ] **Step 2: Run** the owner module with the feature and `timeout 900 cargo test -p aq-supervisor-host --features supervisor_full_access --lib plan`. Expected: FAIL.
- [ ] **Step 3: Implement** the four changes in Interfaces.
- [ ] **Step 4: Run** the `supervisor_turns::` filter with and without the feature, aq-supervisor-host `--lib` with and without the feature, `-p aq-supervisor-control` with and without the feature, it_server `commands::` with and without the feature, and `python3 scripts/build-budget.py`. Then run `timeout 900 cargo clippy -p aq-server -p aq-supervisor-host --features aq-supervisor-host/supervisor_full_access --all-targets --locked -- -D warnings`. Expected: all PASS, budget OK, clippy clean.
- [ ] **Step 5: Commit** with message `feat(supervisor): project supervisors keep their project narrowing on dedicated routes`.

### Task 3: Rehearse and redeploy (controller, operational)

Follow `~/.local/share/aq-ms1/supervisor-rollout-20261120/rehearsal-162/NOTES.md` and `deploy-sfa/`.

- [ ] **Step 1: Rebuild both images** from the branch head with the same build args as `ms1-sfa-0f014572`, plus `CARGO_FEATURES=supervisor_full_access`. For the supervisor image, the `aq-supervisor-host` features are `delivery_playbook,supervisor_full_access`. Tag both `ms1-sfa-<shortsha>`.
- [ ] **Step 2: Restart the stopped `aq-ms1-sfa162-rehearsal-*` stack on the new server image.** In it, a project supervisor credential must:
  - get 403 for `dashboard.swarm` and for a `task.activate` in another project;
  - succeed for `task.comment.add` in its own project.

  The global supervisor credential must still pass `dashboard.swarm`. Stop the stack afterward.
- [ ] **Step 3: Redeploy production.** Run `deploy-sfa/deploy-server.py` with `OLD` and `NEW` updated. Then run `deploy-sfa/upgrade-supervisors.py` with `previous_image = f36dd0c3…`, the new image, and the backup suffix `-before-narrow`.
- [ ] **Step 4: Live check.** The global supervisor answers the read-only check as before (156 commands, swarm, supervisor list). One project supervisor reports that `dashboard.swarm` is refused and that `task.list` for its project works. Record the results in PR #220.
