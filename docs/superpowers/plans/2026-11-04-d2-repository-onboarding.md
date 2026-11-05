# D2 Repository Onboarding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In one owner operation, onboard a GitHub repository into a central aq-git binding with a retained base, an AQ project binding, a default branch and a publication policy.

**Architecture:** A new module `repository_onboarding` holds a durable, resumable state machine. It persists each step under the caller's `operation_id` in one table: GitHub metadata, then `repo.register`, then `repo.sync`, then the shape pre-check, then the project binding, and the project binding step reuses `project_bindings::put_binding`. The admin route and an `aq-server onboard-repository` subcommand both call the same `onboard()` function. Credentials are vault-held profiles that are only ever named, never sent as secrets.

**Tech Stack:** Rust, axum, sqlx/PostgreSQL, reqwest (GitHub REST), aq-git v1 Unix socket protocol.

**Spec:** task crisp-meadow-36.3 (D2) description; `docs/specs/0003*` (bindings name `aq.git.configured`/`aq.graph.configured`, no caller endpoints/tokens/commands, 128 KiB admin cap).

## Global Constraints

- Admin request body ≤ 128 KiB.
- The caller supplies the GitHub `owner`/`name`, never a URL. The server builds `ssh://git@github.com/{owner}/{name}.git` or `https://github.com/{owner}/{name}.git`.
- The GitHub API base and the credential profiles come only from vault `config.toml` `[onboarding]`.
- No secret may appear in the binding JSON, the onboarding row, the logs or any response. Only the profile name is persisted.
- Profile fit: SHA-1 only and complete history. Refuse `.gitattributes`, LFS, gitlinks/submodules, shallow/partial history and required signing, each with a typed `reason`.
- While aq-git answers `unsupported_feature` for `credential_profile`, onboarding refuses with `credential_profile_unavailable`.
- Migration `0079_repository_onboarding.sql` with `UPDATE meta SET value='79'`. Renumber against origin/main before pushing.

## Review Focus

- Retrying the same `operation_id` after a mid-way failure resumes and creates no second binding. Test: `onboarding_resumes_after_partial_failure`.
- The same `operation_id` with a different request is a conflict, not a replay. This is covered inside the resume test.
- Owner `default_branch` override beats the GitHub metadata. This is covered by the acceptance test (override absent) and asserted in the resume test (override present).
- A refused onboarding is terminal and replayable: the same op returns the same refusal.
- A credential secret never appears in the persisted row. The acceptance test asserts that the row JSON has no `secret`.

---

### Task 1: Onboarding module, migration, route, acceptance tests

**Files:**
- Create: `crates/aq-server/src/repository_onboarding.rs`, `crates/aq-server/migrations/0079_repository_onboarding.sql`
- Modify: `crates/aq-server/src/project_bindings.rs` (extract `put_binding`), `crates/aq-server/src/lib.rs` (mod + route), `crates/aq-server/src/config.rs` (`onboarding: Option<OnboardingConfig>`)
- Test: `crates/aq-server/tests/repository_onboarding.rs` (uses `common::selection::setup(false)` with its own fake aq-git and fake GitHub)

**Interfaces:**
- Produces:
  - `pub async fn onboard(state: &AppState, actor: Uuid, req: OnboardRequest) -> Result<Value, AppError>`
  - `POST /api/repositories/onboard`
  - `GET /api/repositories/onboard/{operation_id}`
  - The response is `{operation_id, step, disposition: "bound"|"refused"|"incomplete", project_id, repository_id, default_branch, base:{ref, oid, observation_id}, refusal?:{code, reason, source, message}, error?:{code, message}, project?}`.
- `OnboardRequest{operation_id, project_id, repository_id, github:{owner, name}, transport:"ssh"|"https", credential_profile:Option<String>, default_branch:Option<String>, publication:{mode, wait_for_checks?}, template:Value}`.
  - `template` holds the ProjectBinding fields other than `project_id`, `repository_id`, `repository_binding_generation`, `repository_adapter_reference` and `publication`. The server fills those in. Supplying any of them is a 400.
- `OnboardingConfig{github_api_url: String (default https://api.github.com), credential_profiles: BTreeMap<label, {kind: ssh_deploy_key|https_fine_grained_token, secret_file: PathBuf}>}`.

- [ ] **Step 1: Write the failing tests**
  - `onboard_github_repo_creates_binding_and_retained_base`. The fake GitHub reports `default_branch:"trunk"` with no rules, and the fake aq-git answers register, sync, tree and show.
    - Assert `disposition=="bound"` and `default_branch=="trunk"`.
    - Assert that `base.oid` is the fake head and `base.ref=="refs/heads/trunk"`.
    - Assert that `GET /api/projects/{id}` returns revision 1, `repository_adapter_reference=="aq.git.configured"` and `publication.target=="refs/heads/trunk"`.
    - Assert that the recorded register params had `mode=="central"`, `remote_url=="ssh://git@github.com/acme/widgets.git"`, `allowed_refs==allowed_targets==["refs/heads/trunk"]` and no credential field.
    - Assert that replaying the same op returns the same body.
  - `unsupported_repo_profile_refused_with_reason`:
    - (a) The head tree contains `.gitattributes`, so the result is `refusal.reason=="gitattributes"` with `source=="precheck"`.
    - (b) The tree contains a mode `160000` entry, so the result is `reason=="submodules"`.
    - (c) GitHub rules contain `required_signatures`, so the result is `reason=="required_signing"`.
    - (d) The request names a configured `credential_profile`, aq-git answers `unsupported_feature`, and the result is `code=="credential_profile_unavailable"` with `source=="aq-git"`.
    - In every case no project binding exists (`GET /api/projects/{id}` is 404).
  - `onboarding_resumes_after_partial_failure`:
    1. The first attempt fails at the binding step, so the response is `incomplete` with `step=="checked"`.
    2. Fix the cause and retry the same op. The response is `bound`, and register was received exactly once.
    3. Send the same op with a changed body. The response is 409.

- [ ] **Step 2: Run them and verify they fail to compile**
  - Run `cargo test --locked -p aq-server --test repository_onboarding`.

- [ ] **Step 3: Implement**
  1. Migration table `repository_onboardings`:
     - `operation_id uuid PK`, `actor_id uuid`, `request_digest text`, `project_id uuid`, `repository_id uuid`;
     - `step text CHECK IN ('started','registered','synced','checked','bound','refused')`;
     - `state jsonb`, `created_at_ms bigint`, `updated_at_ms bigint`.
  2. Each step updates `step` and `state` before the next one begins.
  3. aq-git errors map to typed results:
     - `unsupported_feature` → refusal (source `aq-git`);
     - `binding_conflict` on register → refusal `repository_already_registered`;
     - transport or other errors → `incomplete` (retryable).
  4. Pre-check: run a non-recursive aq-git `tree` (limit 1000) on the head. `.gitattributes` and `.lfsconfig` → `gitattributes`/`lfs`; `.gitmodules` or a `160000` mode → `submodules`.
- [ ] **Step 4: Run the tests and verify they pass.** Run the same command as Step 2.
- [ ] **Step 5: Commit.**

### Task 2: CLI subcommand

**Files:** Modify `crates/aq-server/src/main.rs`.

- [ ] Add `aq-server onboard-repository --vault <dir> --owner-email <email> --request <file|->`.
  1. Load the vault and connect `DATABASE_URL`, as `serve` does.
  2. Resolve the owner user by email (role owner); otherwise exit 2.
  3. Build `AppState` using the new `aq_server::build_state(config, store)`, which is extracted from `run`.
  4. Call `onboard` and print the JSON.
- The binary stays `aq-server`. Nothing is installed as `aq` or `agent-queue`.
- Verify with `cargo build --locked -p aq-server`.

### Task 3: Evidence

- [ ] Write `docs/implementation/D2.md` with the commands and results, the profile-fit table, and the CROSS-REPO FOLLOW-UP (aq-git) credential-profile note. Post that note as a task comment.
