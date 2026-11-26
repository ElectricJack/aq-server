# Supervisor code read Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Supervisors fetch project repositories read-only through a new server route and read them in their workspace. The global supervisor can read every project's repository; a project supervisor can read only its own.

**Architecture:**
- A new machine-listener route `/v1/supervisor/git/{repository}/{*path}`, in `aq-supervisor-control`, authenticates the supervisor bearer and checks repository authority. It forwards fetch-only smart-HTTP to aq-git's socket with a fixed read grant, using a new forwarder in `aq-delivery`.
- A host tool `repo sync` clones or fetches into `/workspace/repos/<name>`.
- The image gains git and ripgrep, and the instructions gain a code-reading paragraph.

**Tech Stack:** Rust, axum 0.8, hyper over a unix socket (the existing `aq_delivery::git_grant` exchange), git smart-HTTP, cargo-nextest, Docker.

**Spec:** `docs/superpowers/specs/2026-11-20-supervisor-code-read-design.md`

## Global Constraints

- The route and the tool exist only with `supervisor_full_access` AND `delivery_playbook`. With either off, behavior is unchanged. `aq-supervisor-control` and `aq-supervisor-host` gain a `delivery_playbook` feature that forwards appropriately.
- Allowed requests are only `GET info/refs?service=git-upload-pack` and `POST git-upload-pack`. Anything else gets 403 `Supervisors can only fetch`. A repository outside the supervisor's authority gets 403 `Supervisor can read only its projects' repositories`. A missing or invalid bearer gets 401.
- The read grant headers are exactly `aq-base: 0000000000000000000000000000000000000000`, `aq-task-ref: refs/heads/aq/supervisor-read`, `aq-max-push-bytes: 1`, and a fresh UUID `aq-operation`.
- Checkouts go in `/workspace/repos/<project-name>`. The bearer is passed to git only via `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` (`http.extraHeader=Authorization: Bearer …`), never argv, a URL or a file.
- Zero net lines in `crates/aq-server/src`. `python3 scripts/build-budget.py` must print OK.
- No new test binaries. Run cargo tests under `timeout 900`, with `eval "$(scripts/test-db)"` for integration tests. The commit trailer is `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. Fetch with `Content-Encoding: gzip` (git sends it for large negotiations): upload-pack must still pass through. The node proxy only refuses encoded bodies for push. Test in Task 1.
2. `info/refs?service=git-receive-pack` (the push discovery step) gets 403, not just `POST git-receive-pack`. Test in Task 1.
3. A repository UUID that is bound to no project gets 403, even for the global supervisor. Test in Task 1.
4. An expired or revoked supervisor credential gets 401 in the middle of a clone, since the credential is checked per request. Test in Task 1.
5. `repo sync` run twice gives a fetch, not a re-clone, and keeps local changes untouched. Test in Task 2.

---

### Task 1: The supervisor git route

**Files:**
- Modify: `crates/aq-delivery/src/git_grant.rs` (add a read forwarder next to `serve`)
- Create: `crates/aq-supervisor-control/src/git.rs` (route, auth, authority)
- Modify: `crates/aq-supervisor-control/src/lib.rs` (`machine_routes` mounts the git route under both features; it takes the git socket path)
- Modify: `crates/aq-server/src/manual_agents.rs` (`supervisor_nests` passes `state.source_retention`'s git socket, as `profiled_execution::git_grant::socket` does)
- Modify: `crates/aq-server/src/admission.rs` (`/v1/supervisor/git/` takes the same lane as `/v1/git/`, net zero lines)
- Modify: `protocol/commands/v1/routes.json` (reviewed `supervisor` route `/v1/supervisor/git/{repository}/{*path}`)
- Modify: `Cargo.toml` features of `aq-supervisor-control` (`delivery_playbook = ["aq-delivery/delivery_playbook"]` if needed) and the `aq-server` feature fan-out
- Test: `crates/aq-delivery/src/git_grant/tests.rs` (or the existing tests module); `crates/aq-server/tests/it_dashboard/supervisor_turns/owner.rs` (flag-on module; add `#[cfg(feature = "delivery_playbook")]` tests)

**Interfaces:**
- Produces, in `aq_delivery::git_grant`:
  - `pub const READ_BASE: &str = "0000000000000000000000000000000000000000"`
  - `pub const READ_TASK_REF: &str = "refs/heads/aq/supervisor-read"`
  - `pub async fn serve_read<B>(pool: &PgPool, socket: &Path, repository: Uuid, path: &str, request: Request<B>) -> Result<Response<Incoming>, Refusal>`. It accepts only the two fetch routes; any other route gets `Refusal::Forbidden("Supervisors can only fetch")`. It forwards like `serve`, but with the read grant headers, `aq-max-push-bytes: 1`, and the `git_transfer_timeout_ms` deadline. It never reads or limits the body as a push.
- Produces, in `aq_supervisor_control::git`: `pub fn routes<S>(pool: PgPool, clock: crate::read::Clock, socket: PathBuf) -> axum::Router<S>`, mounting `/v1/supervisor/git/{repository}/{*path}`. Its authority check, `pub async fn may_read(pool: &PgPool, project: Option<Uuid>, repository: Uuid) -> Result<bool, sqlx::Error>`, returns true iff some binding has `binding->>'repository_id' = repository`, and that binding's `project_id` equals `project` when `project` is `Some`.
- Consumes: `crate::read::bearer(pool, clock, "owner")` (the `Permit` has `project`); `aq_delivery::git_grant::{serve_read, Refusal}`.

- [ ] **Step 1: Write the failing unit tests** in aq-delivery, using the existing fake or socket test harness for `serve` if there is one; otherwise test the route classifier.
  - `serve_read_refuses_push_routes`: `POST git-receive-pack` and `GET info/refs?service=git-receive-pack` each give `Forbidden("Supervisors can only fetch")` (Review Focus 2).
  - `serve_read_sends_the_read_grant`: against a fake socket, the forwarded request carries exactly the four read-grant headers and `/{repository}/{path}`.
- [ ] **Step 2: Write the failing integration tests** (flag on and `delivery_playbook`), with a fake aq-git socket like the node-proxy tests use. Find them by grepping it_* for `/v1/git/`.
  - `global_supervisor_fetches_any_bound_repository`: 200 for info/refs on project A's repository and on project B's repository.
  - `project_supervisor_fetches_only_its_repository`: 200 on its own repository, 403 on the other project's.
  - `supervisor_git_refuses_unbound_repositories_and_pushes`: an unbound UUID gives 403 (Review Focus 3), and `POST git-receive-pack` gives 403.
  - `supervisor_git_requires_a_live_credential`: no bearer gives 401; a revoked credential gives 401 (Review Focus 4).
  - `supervisor_git_passes_gzip_fetch_bodies`: `POST git-upload-pack` with `Content-Encoding: gzip` reaches the fake socket with the body unchanged (Review Focus 1).
- [ ] **Step 3: Run** `timeout 900 cargo nextest run -p aq-server --features supervisor_full_access,delivery_playbook --test it_dashboard -E 'test(/^supervisor_turns::owner::.*git/)'`, plus the aq-delivery unit tests. Expected: FAIL.
- [ ] **Step 4: Implement** per Interfaces. Mount the route only with both features. Add the reviewed entry to `routes.json`, and the admission lane change at net zero lines.
- [ ] **Step 5: Run** the new tests (expect PASS). Then run the supervisor_turns filter with no features, with `supervisor_full_access`, and with both. Run it_server `commands::` with none and with both, then `python3 scripts/build-budget.py` (expect OK). Finally run clippy `-D warnings` over the workspace with default features and with `--features aq-server/delivery_playbook,aq-supervisor-host/supervisor_full_access`.
- [ ] **Step 6: Commit** with message `feat(supervisor): read-only git fetch route for supervisors`.

### Task 2: `repo sync`, image tools and instructions

**Files:**
- Create: `crates/aq-supervisor-host/src/repo.rs`
- Modify: `crates/aq-supervisor-host/src/main.rs` (dispatch `repo` like `tool`, under both features)
- Modify: `crates/aq-supervisor-host/src/service/plan.rs` (flag-on instruction body gains the spec §4 sentence verbatim)
- Modify: `Dockerfile.supervisor-host` (final stage installs `git` and `ripgrep`)
- Modify: `crates/aq-supervisor-host/Cargo.toml` (the `delivery_playbook` feature forwards to `aq-supervisor-control/delivery_playbook` if Task 1 added it)
- Test: `crates/aq-supervisor-host/src/repo.rs` `#[cfg(test)]`; `plan.rs` tests

**Interfaces:**
- Consumes: the route from Task 1 at `{AQ_SUPERVISOR_ORIGIN}v1/supervisor/git/{repository_id}`, and the command call path that `tool.rs` uses, to run `project.show`.
- Produces:
  - `pub async fn sync(project: Uuid, workspace: &Path) -> anyhow::Result<PathBuf>`;
  - `pub fn git_env(token: &str) -> Vec<(String, String)>`, which returns `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=http.extraHeader` and `GIT_CONFIG_VALUE_0=Authorization: Bearer <token>`;
  - `pub fn checkout_dir(workspace: &Path, project_name: &str) -> PathBuf`, which gives `workspace/repos/<sanitized name>`, with path separators and `..` refused.

- [ ] **Step 1: Write the failing unit tests:**
  - `git_env_carries_the_bearer_only_in_env`: exact three pairs.
  - `checkout_dir_sanitizes_names`: `"aq-server"` gives `/w/repos/aq-server`; `"../x"` and `"a/b"` are refused or sanitized to a single segment.
  - `sync_clones_then_fetches` (Review Focus 5): against a local bare repository served by `git http-backend` or a file URL fixture, using an injectable remote, the first call clones and the second call fetches. A local untracked file survives the second call.
  - The `plan.rs` instruction test asserts the new sentence is present with the flag on and absent with it off.
- [ ] **Step 2: Run** `timeout 900 cargo test -p aq-supervisor-host --features supervisor_full_access,delivery_playbook --lib repo plan`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - `repo sync PROJECT_ID` prints `<path> <main-head-sha>`.
  - Clone with `git clone --origin origin <url> <dir>`, then `git fetch origin +refs/heads/aq/*:refs/remotes/origin/aq/*`. On later calls run `git fetch origin +refs/heads/main:refs/remotes/origin/main +refs/heads/aq/*:refs/remotes/origin/aq/*`, then `git checkout -q --detach origin/main` only when the worktree is clean; otherwise leave it and say so.
  - Read the token from `AQ_SUPERVISOR_CREDENTIAL_FILE` as `tool.rs` does.
  - Add `git ripgrep` to the final-stage `apt-get install` in `Dockerfile.supervisor-host`.
- [ ] **Step 4: Run** the crate's lib tests with no features and with both. Run the Dockerfile build syntax check, `docker build --target <final> --check` or a full build if it's under 15 minutes. Run `python3 scripts/build-budget.py`. Expected: PASS and OK.
- [ ] **Step 5: Commit** with message `feat(supervisor): repo sync tool, git and ripgrep in the image, code-reading instructions`.

### Task 3: Rehearse, PR and roll out (controller)

- [ ] **Step 1: Open a PR** from `supervisor-code-read` once both tasks are reviewed, and wait for green CI.
- [ ] **Step 2: Build both images** like `~/.local/share/aq-ms1/supervisor-rollout-20261120/rehearsal-narrow/build/`. The ms1 supervisor overlay Dockerfile adds `RUN apt-get update && apt-get install -y --no-install-recommends git ripgrep && rm -rf /var/lib/apt/lists/*` before copying the binary.
- [ ] **Step 3: Rehearse in isolation.** On the schema-162 rehearsal with an aq-git container holding a copy of one repository, check:
  - the global supervisor syncs and reads a file;
  - a project supervisor syncs its own repository and gets 403 for another;
  - `git push` gets 403.
- [ ] **Step 4: Redeploy production** with `deploy-narrow`-style scripts (server, then supervisors). Live check: the global supervisor syncs aq-server and quotes the first line of `README.md`.
- [ ] **Step 5: Merge the PR** after the owner's go-ahead.
