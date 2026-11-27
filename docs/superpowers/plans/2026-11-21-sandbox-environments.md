# Sandbox Environments (spec 0017, steps 4–11) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A project's toolchain is a recipe in its own repository; AQ builds,
scans, publishes and admits it through the delivery playbook, and places each
task in an environment that provides what it needs.

**Architecture:** aq-worker gains a recipe format, `environment build` and
`environment try` in its template tool, a builder job runner, and per-offer
template selection from environments the server publishes. aq-server stores
environment versions and builds in the database behind one accessor, queues
build and try jobs for builder nodes, exposes `environment.try` to agents and
`environment.build` / `environment.admit` to playbooks, and lets the router
choose an environment. aq-playbooks publishes host catalog 2.5.0 with those
commands. The dashboard shows environments and task needs.

**Tech Stack:** Rust (aq-server, aq-worker, aq-playbooks), Python 3.12 (the
template tool), PostgreSQL 17, Docker Buildx, pinned Skopeo, React + vitest +
Playwright (aq-dashboard).

**Spec:** `aq-server/docs/specs/0017-sandbox-environments.md` (PR #203).
Executors read the spec first; this plan cites its sections.

## Global Constraints

- Every new code path is behind the compile-time cargo feature
  `project_environments` (aq-server has it since #206; aq-worker adds
  `project_environments = []` to `crates/aq-worker/Cargo.toml` in Task 6). It
  is never a Setting. The dashboard gates on the server capability
  `environments.read`, not a flag.
- Limits are Settings, never constants. Every bound in spec §6.7 comes from the
  server Setting `environment_limits` (Task 10) and reaches the node inside each
  job.
- Policy lives in playbooks and Settings documents. Server and worker code
  provide operations and refuse impossible placements only (spec §5.4).
- Environment ids are `<project>.<name>`; labels match `[A-Za-z0-9-_.@:]`
  (`aq-project-policy/src/lib.rs:177-186`). Recipe names match
  `[a-z0-9][a-z0-9-]{0,63}`.
- Recipes live at `.aq/environments/<name>.md` in the project's repository.
- Nothing is pushed to a registry before the credential scan passes. Publish
  with the pinned Skopeo image, `copy --preserve-digests`.
- Rolling records: keep the last N admitted versions per environment (N is in
  `environment_limits`); never prune a version an attempt still uses.
- aq-server: no new test binary; new integration tests go in existing binaries
  (aq-server `AGENTS.md`). aq-server lib lines may not grow past the ratchet:
  new logic goes in crates (`aq-project-policy`, a new `aq-environments`
  crate). Run every `cargo test`/`cargo nextest` under `timeout 900`.
- aq-server migrations are numbered without gaps; renumber with
  `python3 scripts/renumber-migrations.py` after merging main.
- Worktrees: `git -C <repo> worktree add -b <branch> "/home/jkern/dev/aq-platform/wt/<name>" origin/main`.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`;
  PR bodies end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.

## Review Focus

1. Every delivery calls `environment.build`, and almost every pull request
   changes no recipe: the `unchanged` outcome must return without queuing a
   builder job (Task 13: `build_with_no_changed_recipe_completes_unchanged_without_a_job`).
2. Two pull requests that change the same recipe merge one after the other:
   versions stay ordered and the project default ends on the later merge, even
   if the earlier admit completes last (Task 13:
   `a_late_admit_never_replaces_a_newer_version`).
3. A newly admitted environment no node has pulled yet: the old version keeps
   serving, the new one is not offered until a node reports it ready (Task 11:
   `an_admitted_environment_is_offered_only_after_a_node_reports_it_ready`).
4. A build log echoes a secret: the log the agent and the playbook see is
   redacted (Task 4: `test_build_log_redacts_provider_keys`).
5. An agent loops on `aq environment try`: concurrent try jobs per project are
   capped by `environment_limits.try_concurrency`, and the extra request is
   refused, not queued without bound (Task 12:
   `environment_try_beyond_the_concurrency_limit_is_refused`).

## Shared shapes

These JSON shapes cross repositories. Tasks name them; field names are exact.

**Parsed recipe** (Task 3, `parse_environment`):
`{"name": str, "default": bool, "provides": [str], "install_steps": [[str]], "checks": [str], "warm": [str], "sha256": str}`.
`install_steps` are argv arrays (each Markdown list item split with
`shlex.split`); `checks` and `warm` are shell command strings; `sha256` is of
the file's bytes.

**Limits** (`environment_limits` Setting, Task 10; inside every job, Tasks 5–6):
`{"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096, "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000, "build_cpu_millis": 4000, "build_memory_bytes": 8589934592, "log_bytes": 65536, "try_concurrency": 2, "keep_versions": 3}`
(the values are the Setting's defaults).

**Job** (server → builder node, `POST /v1/worker/environments/claim`):
`{"job_id": uuid, "kind": "build"|"try", "project_id": uuid, "project": str, "commit": sha40, "recipes": [str], "run": str|null, "base_template": "repo@sha256:…", "target_repository": str, "limits": Limits}`.

**Job result** (builder node → server, `POST /v1/worker/environments/complete`):
`{"job_id": uuid, "outcome": "built"|"failed", "builds": [Build], "failure": {"recipe": str, "step": "parse"|"install"|"check"|"scan"|"publish"|"run", "log": str}|null, "run": {"exit": int, "output": str}|null}`,
where `Build` is `{"name", "digest", "image", "recipe_sha256", "provides", "toolchain": [{"command", "output"}], "warm", "harnesses": [{"harness", "version"}]}`.
A `try` job never has `builds`; it has `run`.

**Admitted environments** (server → any node, `GET /v1/worker/environments`):
`{"environments": [{"id": str, "version": str, "digest": str, "image": str, "harnesses": [{"harness", "version"}], "warm": [str]}]}`.
Ready report (`POST /v1/worker/environments/ready`): `{"ready": [digest]}`.

---

## Phase 1: aq-worker template tool

Worktree `wt/env-worker`, branch `feat/project-environments-worker`, from
aq-worker origin/main. Python tests run with
`python3 scripts/tests/test_template_tool.py TemplateTests.<name>`; the Rust
wrapper is `timeout 900 cargo test -p aq-worker --test templates`. Tests fake
Docker with the `docker_binary` script pattern at
`scripts/tests/test_template_tool.py:392-415`.

### Task 1: Builds leave no Node compile cache

**Files:**
- Modify: `packaging/sandbox/template_tool.py` (`dockerfile()`, `:82-104`)
- Test: `scripts/tests/test_template_tool.py`

**Interfaces:**
- Produces: generated Dockerfiles that set `ENV NODE_DISABLE_COMPILE_CACHE=1`
  before the first install `RUN`.

- [ ] **Step 1: Write the failing test**

```python
def test_dockerfile_disables_node_compile_cache_before_install_steps(self):
    text = tool.dockerfile(recipe_with_steps([["sh", "-c", "true"]]), config())
    env = text.index("ENV NODE_DISABLE_COMPILE_CACHE=1")
    assert env < text.index("RUN [\"sh\"")
```

- [ ] **Step 2: Run it; expect FAIL** (`ValueError: substring not found`).
- [ ] **Step 3: Emit the `ENV` line right after `FROM` in `dockerfile()`.**
  The scanner refuses a V8 compile cache under `/tmp/node-compile-cache`
  (spec §2); this stops any install step from writing one.
- [ ] **Step 4: Run the test and the existing suite; expect PASS.**
- [ ] **Step 5: Commit** `Stop template builds leaving a Node compile cache`.

### Task 2: The scanner's `sk-` pattern gets a left boundary

**Blocked on the owner's answer to spec §9 question 3.** Do not start it
before that answer is recorded in the spec.

**Files:**
- Modify: `packaging/sandbox/credential_scan.py:24` (`TOKEN_PATTERN`)
- Test: `scripts/tests/test_template_tool.py`

- [ ] **Step 1: Write the failing tests**

```python
def test_task_identifier_is_not_a_provider_key(self):
    assert scan_text(b"task-0123456789abcdefghijklmnop") == []

def test_provider_key_after_a_separator_still_refuses(self):
    for prefix in (b"", b" ", b"=", b"\"", b":", b"\n"):
        assert scan_text(prefix + b"sk-0123456789abcdefghijklmnop") != []
```

(`scan_text` is the module's content check on one byte string; use the
existing helper that `scan_content` calls.)

- [ ] **Step 2: Run them; expect the first to FAIL.**
- [ ] **Step 3: Require start-of-text or a non-alphanumeric byte before `sk-`**
  (a lookbehind `(?<![A-Za-z0-9])`). Change nothing else in the pattern.
- [ ] **Step 4: Run the whole scanner suite; expect PASS** (every existing
  refusal case still refuses).
- [ ] **Step 5: Commit** `Let the credential scanner tell task identifiers from provider keys`.

### Task 3: The recipe format and a parent from config

**Files:**
- Create: `packaging/sandbox/environment_recipe.py`
- Modify: `packaging/sandbox/template_tool.py` (`validate_config` `:107`,
  `parent_image` `:35-44`), `crates/aq-worker/src/template.rs` (embed the new
  module with `include_bytes!` like the others)
- Test: `scripts/tests/test_environment_recipe.py` (new; add one Rust case to
  `crates/aq-worker/tests/templates.rs` that runs it)

**Interfaces:**
- Produces: `parse_environment(path: Path, limits: dict) -> dict` (the Parsed
  recipe shape); `validate_config` accepts optional `base_template`
  (`repo@sha256:…`); `parent_image(recipe, config)` returns
  `config["base_template"]` for a parsed recipe.

- [ ] **Step 1: Write the failing tests**

```python
def test_parses_front_matter_and_sections(self):
    env = parse_environment(fixture("web.md"), LIMITS)
    assert env["name"] == "web" and env["default"] is True
    assert env["provides"] == ["node", "pnpm", "playwright", "python"]
    assert env["install_steps"][1] == ["sh", "-c", "corepack enable && corepack prepare pnpm@10.34.5 --activate"]
    assert env["checks"] == ["node --version", "pnpm --version", "npx playwright --version"]
    assert env["warm"] == ["pnpm install --frozen-lockfile"]

def test_refuses_a_parent(self):  # spec §6.1: AQ supplies it
    with self.assertRaisesRegex(ValueError, "parent"):
        parse_environment(fixture_with("parent: x@sha256:" + "a"*64), LIMITS)

def test_bounds_come_from_limits(self):
    with self.assertRaisesRegex(ValueError, "install steps"):
        parse_environment(fixture("web.md"), {**LIMITS, "max_install_steps": 2})

def test_refuses_unknown_front_matter_duplicate_sections_and_bad_names(self): ...
```

`fixture("web.md")` is the recipe in spec §6.1, with the Node step written out
as a real pinned download.

- [ ] **Step 2: Run them; expect FAIL** (module missing).
- [ ] **Step 3: Implement `parse_environment`.** Front matter keys exactly
  `name`, `default`, `provides`; sections exactly `Install`, `Check`, `Warm`
  (Install required). Apply the existing control-character and
  credential-shape refusals from `validate_recipe` to every string. Labels in
  `provides` use the server's label rule.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Parse project environment recipes`.

### Task 4: Checks and a bounded, redacted build log

**Files:**
- Modify: `packaging/sandbox/template_tool.py` (`build` `:218`, the buildx
  call `:249-256`)
- Test: `scripts/tests/test_environment_recipe.py`

**Interfaces:**
- Consumes: Task 3's parsed recipe.
- Produces: `build_environment(env: dict, config: dict, output: Path) -> dict`
  returning a `Build` (Shared shapes) or raising `BuildFailed(step, log)`.

- [ ] **Step 1: Write the failing tests**

```python
def test_check_output_is_recorded_as_toolchain(self):
    build = build_environment(parse(...), config_with_fake_docker(), out)
    assert build["toolchain"][0] == {"command": "node --version", "output": "v24.22.3\n"}

def test_a_failing_check_fails_the_build_at_check(self):
    with self.assertRaises(BuildFailed) as e: ...
    assert e.exception.step == "check"

def test_build_log_redacts_provider_keys(self):
    # fake docker prints "sk-0123456789abcdefghijklmnop" to stderr and fails
    assert "sk-0123" not in e.exception.log and "[redacted]" in e.exception.log

def test_build_log_is_bounded_by_limits(self):
    assert len(e.exception.log.encode()) <= LIMITS["log_bytes"]
```

- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** Generate a two-stage Dockerfile: stage
  `template` (today's assembly with `base_template` as `FROM`), and stage
  `checks` `FROM template` that runs each check as
  `sh -c '<cmd> > /aq-checks/<i>.out 2>&1; echo $? > /aq-checks/<i>.exit'`,
  then `FROM scratch AS check-output` copying `/aq-checks`. Run buildx twice
  with the same builder: `--target template --output type=oci,dest=…` and
  `--target check-output --output type=local,dest=…`. Capture stderr to a file
  instead of `DEVNULL`, redact with the scanner's token and assignment
  patterns (`[redacted]`), keep the last `log_bytes`. Keep the scan unchanged.
  Distinguish a buildx failure (`install`) from a scan refusal (`scan`); today
  both read "credential scan failure" (`:255-258`).
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Run environment checks at build and return a redacted log`.

### Task 5: `environment build` and `environment try`

**Files:**
- Modify: `packaging/sandbox/template_tool.py` (subcommands next to `:318-332`),
  `crates/aq-worker/src/bin/aq-worker/main.rs` (`TemplateCmd` `:222-233`,
  dispatch `:417-438`)
- Test: `scripts/tests/test_environment_recipe.py`, `crates/aq-worker/tests/templates.rs`

**Interfaces:**
- Produces CLI:
  `aq-worker template environment-build --job <job.json> --repo <dir> --build-config <builder.json> --output <dir>` and
  `aq-worker template environment-try --job <job.json> --repo <dir> --build-config <builder.json>`,
  each printing one Job result (Shared shapes) on stdout. Neither writes the
  allowlist (admission moves to the server).

- [ ] **Step 1: Write the failing tests**

```python
def test_build_publishes_only_after_the_scan(self):
    # fake skopeo records calls; a refused scan makes no skopeo call
def test_build_publishes_with_preserve_digests_and_reports_the_scanned_digest(self):
    assert skopeo_argv[:3] == ["copy", "--preserve-digests", f"oci-archive:{archive}"]
    assert result["builds"][0]["image"] == f"{repo}@{result['builds'][0]['digest']}"
def test_try_runs_the_command_in_the_built_image_and_exports_nothing(self):
    assert result["run"] == {"exit": 0, "output": "ok\n"} and result["builds"] == []
    assert not any(a.startswith("type=oci") for a in docker_argv)
def test_try_reports_a_failing_command_without_failing_the_job(self):
    assert result["outcome"] == "built" and result["run"]["exit"] == 1
```

- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** Build reads each named recipe from `--repo` at
  the job's commit (the caller checks it out), runs `build_environment`, then
  publishes with `config["skopeo_image"]` (pinned by digest) via `docker run`
  of that image, `copy --preserve-digests oci-archive:<archive>
  docker://<target_repository>@<digest>`. Try adds a stage `FROM template`
  that `COPY`s the repository (the build context is `--repo`, excluding
  `.git`) and runs `run` the same way checks run, and exports only the local
  output. Apply `build_wall_ms`, `build_cpu_millis` and `build_memory_bytes`
  from the job's limits to the buildx call.
- [ ] **Step 4: Run Python and `timeout 900 cargo test -p aq-worker --test templates`; expect PASS.**
- [ ] **Step 5: Commit** `Add environment build and try to the template tool`.

---

## Phase 2: aq-worker node

Same worktree and branch as Phase 1.

### Task 6: The builder job runner

**Files:**
- Create: `crates/aq-worker/src/environments/{mod.rs,client.rs}`
- Modify: `crates/aq-worker/Cargo.toml` (`project_environments = []`),
  `crates/aq-worker/src/lib.rs`, `crates/aq-worker/src/bin/aq-worker/main.rs`
  (a `RunEnvironments { environments_config }` command next to `RunChecks`
  `:83-88`, `:820`)
- Test: `#[cfg(test)]` in `crates/aq-worker/src/environments/mod.rs`, with an
  axum double as `checks/` does (no new test binary).

**Interfaces:**
- Consumes: Task 5's CLI; server routes from Task 11.
- Produces: `pub async fn run(worker: &WorkerConfig, config: EnvironmentsConfig, shutdown: Shutdown) -> anyhow::Result<()>`
  mirroring `checks::run` (`checks/mod.rs:240`); `EnvironmentsConfig { build_config: PathBuf, work_dir: PathBuf }`.

- [ ] **Step 1: Write the failing tests** (`#[cfg(all(test, feature = "project_environments"))]`):
  `claims_a_build_job_runs_the_tool_and_completes_it`,
  `a_tool_crash_completes_the_job_as_failed_with_a_redacted_log`,
  `shutdown_stops_claiming_and_finishes_the_running_job`.
- [ ] **Step 2: Run** `timeout 900 cargo test -p aq-worker --features project_environments environments::`; expect FAIL.
- [ ] **Step 3: Implement** the claim → checkout → tool → complete loop. Check
  out the job's commit into a fresh worktree of the node's project repository
  (slots are worktrees; never re-clone; see `aq-node-git`), write the job
  JSON, run `template::run` with the Task 5 subcommand, post its stdout. The
  runner uses no attempt slot.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Run environment build and try jobs on a builder node`.

### Task 7: Nodes run admitted environments

**Files:**
- Modify: `crates/aq-worker/src/backend/docker_local/config.rs` (`templates`
  `:42`, `template_image` `:139`), `crates/aq-worker/src/native/launch/claude_docker.rs`
  (`:38`, `:134-141`, `:555`, `:585`), `crates/aq-worker/src/environments/client.rs`
- Test: `crates/aq-worker/tests/docker_local_backend.rs` (existing binary)

**Interfaces:**
- Consumes: `GET /v1/worker/environments`, `POST /v1/worker/environments/ready` (Task 11).
- Produces: `DockerLocalConfig::admit(&mut self, admitted: &[AdmittedEnvironment])`
  (merges into `templates`, validated like static entries);
  `fn template_for(&self, environment: &ArtifactRef) -> Option<&AllowedTemplate>`.

- [ ] **Step 1: Write the failing tests:**
  `admitted_environments_are_pulled_and_reported_ready`,
  `an_offer_runs_in_the_template_of_its_environment`,
  `an_offer_for_an_unadmitted_environment_is_refused_not_run_in_the_default`,
  `a_template_an_attempt_still_uses_is_not_dropped_when_it_leaves_the_list`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** Poll the admitted list on the presence interval;
  pre-pull new digests with `DockerLocalBackend::pull_template`
  (`docker_local/mod.rs:394-409`), then report them ready. Choose the
  template per offer from `m.harness.environment` instead of the single host
  `template`. Behind `project_environments`; with the feature off the static
  path is unchanged.
- [ ] **Step 4: Run** `timeout 900 cargo test -p aq-worker --features project_environments --test docker_local_backend`; expect PASS.
- [ ] **Step 5: Commit** `Run each attempt in its environment's admitted template`.
- [ ] **Step 6: Open the aq-worker pull request** for Tasks 1, 3–7 (Task 2 in
  its own pull request once approved).

---

## Phase 3: aq-playbooks host catalog 2.5.0

Worktree `wt/env-playbooks`, branch `feat/host-catalog-2-5`. Generator pattern:
`crates/aq-playbooks-cli/tests/catalog_v2_4_contract.rs`.

### Task 8: Catalog 2.5.0

**Files:**
- Create: `crates/aq-playbooks-cli/tests/catalog_v2_5_contract.rs`,
  `contracts/aq-host-2.5.0/{manifest,registry,fixtures,notes}.json`, `README.md`
- Modify: `crates/aq-playbooks-cli/src/catalog.rs:78-83` (`catalog_v2_5()`),
  `crates/aq-playbooks-compiler/src/docs.rs:59-62` (`CATALOG_2_5_*`)

**Interfaces:**
- Produces catalog 2.5.0, built from 2.3.0's registry (the delivery lineage)
  plus 2.4.0's changes, so both the routing and delivery records can pin it.
  Additions:
  - command `environment.build` input `{subject}` (a task: its result head)
    or `{project}` (the project's admitted recipes on the current base, for
    §6.4), async, completes on `aq.environment.completed` with outcome
    `unchanged|built|failed`;
  - command `environment.admit` input `{subject|project, builds}`, async, completes on
    `aq.environment.completed` with outcome `admitted|refused`;
  - event `aq.environment.completed {operation, outcome, builds?, failure?}`;
  - event `aq.environment.base_changed {project}`;
  - `work.route` input gains optional `environment`;
  - `work.read` facts gain `needs`, `default_environment`, and
    `environments: [{id, provides}]` for the project's allowed environments.

- [ ] **Step 1: Write** `generated_catalog_files_are_pinned` for 2.5.0 (copy of
  the 2.4.0 test) and a test that every command the 2.2.0 routing record and
  the 2.3.0 delivery record use is present and unchanged in 2.5.0.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement `build_catalog()` for 2.5.0 and generate:**
  `cargo test -p aq-playbooks-cli --test catalog_v2_5_contract write_catalog_v2_5 -- --ignored`.
- [ ] **Step 4: Run** `cargo fmt --all --check && cargo test --locked --workspace`; expect PASS.
- [ ] **Step 5: Commit** `Publish host catalog 2.5.0 with environment commands`.

### Task 9: Sealed examples

**Files:**
- Create: `contracts/aq-host-2.5.0/examples/deliver-with-environments/{source.md,record.json,catalog.json,facts.json,draft.json}`,
  `crates/aq-playbooks-compiler/authoring-examples/route-to-environment/` (five
  files, `CATALOG.md`, at least 5 `cases/*.json`, `stuck/`)
- Modify: `docs::example_files()` (`docs.rs:351`), `EXAMPLES` in
  `crates/aq-playbooks-cli/tests/authoring_examples.rs` (widen the 2.2.0
  catalog assertion at `:85-108` to accept 2.5.0)

- [ ] **Step 1: Write the example tests:**
  `the_environment_delivery_example_checks_and_seals_against_catalog_2_5_0`
  (pattern: `the_example_delivery_playbook_checks_and_seals_against_catalog_2_3_0`)
  and the routing cases `default_fits`, `fewest_extras`, `nothing_fits_holds`,
  `pinned_profile_wins`, `continuation_keeps_environment`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Write the sources.** The delivery source is the 2.3.0
  example's text plus spec §6.3 steps 1 and 3, and §6.4: on
  `aq.environment.base_changed`, `environment.build {project}`, then
  `environment.admit` on `built`, or `supervisor.notify` on `failed`. The
  routing source states the rule in spec §5.4. Seal both drafts.
- [ ] **Step 4: Run** `cargo test --locked --workspace`; expect PASS.
- [ ] **Step 5: Commit and open the aq-playbooks pull request.**

---

## Phase 4: aq-server

Worktree `wt/env-server`, branch `feat/project-environments-server`. Every
change behind `project_environments`. Test with
`timeout 900 cargo nextest run -p aq-server --features project_environments --test <binary> -E 'test(/^<module>::/)'`.

### Task 10: Environment store, limits Setting and one accessor

**Files:**
- Create: `crates/aq-environments/{Cargo.toml,src/lib.rs,src/store.rs}`;
  migration `crates/aq-server/migrations/<next>_environment_versions.sql`
- Modify: readers of `profiled_execution.definition_bodies` and `[templates]`
  (`compose.rs:294-305`, `profiled_execution/definitions.rs:125-150`,
  `producer/build.rs:331-347`, `placement.rs:285`, and the rest found by
  `git grep -n 'definition_bodies\|\.templates' -- crates/aq-server/src`);
  `aq-role-settings/src/store.rs` (the `environment_limits` Setting with the
  Limits defaults); platform text from the definition (`aq-profiles/src/roles/defaults.rs:38`)
- Test: `it_schema` (migration), `it_native native_offers::environments` (accessor)

**Interfaces:**
- Produces tables `environment_versions (environment_id, version, project_id, template_digest, image, provides jsonb, platform_text, toolchain jsonb, warm jsonb, harnesses jsonb, recipe_sha256, base_digest, admitted_at)`
  with `unique (environment_id, version)`, and `environment_jobs (job_id, kind, project_id, commit, recipes jsonb, run, state, claimed_by, result jsonb, created_at)`.
- Produces `aq_environments::Catalog` with
  `async fn load(pool: &PgPool, boot: &BootEnvironments) -> Result<Catalog>`,
  `fn definitions(&self) -> impl Iterator<Item = (&str, &str, &str)>` (the
  `(id, version, body)` that #206's `provided` takes), `fn template(&self, id: &str) -> Option<&Admitted>`,
  and `fn admitted_for_nodes(&self) -> Vec<AdmittedEnvironment>`.

- [ ] **Step 1: Write the failing tests:** `environment_versions_migration_applies_and_is_idempotent`;
  `boot_definitions_and_admitted_versions_read_through_one_catalog`;
  `an_admitted_version_provides_its_labels_to_placement`;
  `environment_limits_default_to_the_documented_values`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement** the crate, the migration, and switch every reader
  to `Catalog`. Boot entries stay (they seed the base and today's
  environment). Keep aq-server lib lines at or below main (`python3 scripts/build-budget.py`).
- [ ] **Step 4: Run the listed tests and `it_native native_offers::environments`; expect PASS.**
- [ ] **Step 5: Commit** `Store environment versions and read them through one catalog`.

### Task 11: Builder and node routes

**Files:**
- Create: `crates/aq-server/src/environments/worker_routes.rs`
- Modify: the worker router (next to `/v1/worker/checks/*`), `aq-role-settings/src/environments.rs:24`
  (`listed()` also counts ready reports)
- Test: `it_access worker_*` (a new module `worker_environments`)

**Interfaces:**
- Produces `POST /v1/worker/environments/claim` (node credential; node must
  carry the `template-build` role), `POST /v1/worker/environments/complete`,
  `GET /v1/worker/environments`, `POST /v1/worker/environments/ready`, with the
  Shared shapes. Ready digests are stored per node (rolling: replaced on each
  report).

- [ ] **Step 1: Write the failing tests:** `only_a_template_build_node_can_claim`,
  `a_job_is_claimed_once`, `complete_records_the_result_once_and_replays_the_same_answer`,
  `an_admitted_environment_is_offered_only_after_a_node_reports_it_ready`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement** with the `checks` routes as the pattern.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Add builder job and environment routes for nodes`.

### Task 12: `aq environment try` for agents

**Files:**
- Modify: `aq-command-registry` (subject, summary, agent route
  `POST /v1/agent/environments/try` and `GET /v1/agent/environments/try/{job_id}`),
  `aq-agent-grants/src/lib.rs:20-59` (`environment.try` in `op::ALL`, in the
  default coding role's grants), `crates/aq-server/src/agent_access/` (handler,
  pattern `agent_tasks.rs:12-19,22,74`), `crates/aqv2/src/` (CLI
  `aq environment try <name> [--run <cmd>]`), regenerate
  `protocol/commands/v1/commands.json`
- Test: `it_access agent_access`, `it_server commands` (every `/v1/agent/*`
  route is a registry command, `commands.rs` ~297), `aqv2` unit tests

**Interfaces:**
- Consumes: Task 11's job queue.
- Produces: the agent request `{name, run?}`; the server reads the task's
  pushed head commit and refuses with `workspace_not_pushed` when the agent's
  reported head is not on the remote (the agent hands off with the same push
  it uses for results). The response is `{job_id}`; the CLI polls the GET
  route and prints the Job result.

- [ ] **Step 1: Write the failing tests:** `environment_try_queues_a_try_job_for_the_tasks_head`,
  `environment_try_needs_the_grant`, `environment_try_beyond_the_concurrency_limit_is_refused`,
  `environment_try_for_another_project_is_refused`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** The CLI pushes first through the existing handoff
  path in `aqv2` (find it from `aq --help` handoff commands), then posts.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Let agents try an environment recipe`.

### Task 13: `environment.build` and `environment.admit` host commands

**Files:**
- Create: `crates/aq-server/src/playbooks/host_adapters/environment_commands.rs`
- Modify: `host_dispatch` (`playbooks/host_adapters.rs:148-195`),
  `completion_contract` (`requests.rs:27-40`), vendor catalog 2.5.0 into
  `vendor/aq-host-catalog-2.5.0/` with a `CatalogProfile` and fingerprint
  constant (`aq-playbook-domain/src/review_domain.rs:9-40,205-240`),
  `aq-playbook-authoring/src/builtin.rs:69-104` (delivery record: add the two
  commands and the two events, pin 2.5.0)
- Test: `it_playbook playbook_*` (a new module `playbook_environments`)

**Interfaces:**
- Consumes: Task 10 `Catalog`, Task 11 jobs, catalog 2.5.0 (Task 8).
- Produces the commands of spec §6.3 with the `delivery_commands.rs` async
  shape (`accepted {handle}`, a `playbook_host_operations` row, one sealed
  `aq.environment.completed`). `environment.admit` writes the next version per
  build in one transaction, adds the environment to the binding through
  `put_binding` (`project_bindings.rs:159`) and sets `default_environment` for
  the recipe marked `default`. Admitting a base (an `environment_versions` row
  with no project) seals `aq.environment.base_changed {project}` once per
  project that has admitted recipes.

- [ ] **Step 1: Write the failing tests:**
  `build_with_no_changed_recipe_completes_unchanged_without_a_job`,
  `build_queues_one_job_for_the_changed_recipes_and_completes_built`,
  `a_failed_build_completes_failed_with_the_step_and_redacted_log`,
  `admit_records_versions_binds_the_project_and_sets_the_default`,
  `a_late_admit_never_replaces_a_newer_version`,
  `admitting_a_base_emits_base_changed_per_project`,
  `a_project_build_rebuilds_the_admitted_recipes_on_the_current_base`,
  `admit_prunes_to_keep_versions_but_keeps_one_in_use`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement.** "Unchanged" compares the `sha256` of each recipe
  file at the result head with the admitted versions' `recipe_sha256`, read
  through `aq-node-git`/aq-git without a builder job. "Newer" is decided by
  the merge order of the subjects, recorded at admit; a later-merged subject's
  version always wins the default.
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Add environment build and admit host commands`.

### Task 14: The router chooses the environment (spec step 4)

**Files:**
- Modify: `aq-routing/src/lib.rs:136-190` (`Route.environment: Option<String>`,
  `CompactRoute.e` with `#[serde(default)]`), `host_adapters/routing.rs:123-215`
  and `aq-playbook-domain/src/host9.rs:148` (validate against the binding),
  `producer/compose.rs` (`place(binding, pinned, kept.or(routed), needs, …)`),
  `work.read` facts (needs, default and allowed environments with `provides`),
  the routing record in `builtin.rs:29-56` (pin 2.5.0)
- Test: `aq-project-policy environments::tests`, `it_native native_offers::environments`,
  `it_playbook playbook_host9`

- [ ] **Step 1: Write the failing tests:** `a_routed_environment_is_used_when_the_profile_pins_none`,
  `a_continuation_keeps_its_environment_over_the_route`,
  `a_routed_environment_outside_the_binding_is_refused`,
  `a_routed_environment_missing_a_need_holds_the_task`.
- [ ] **Step 2: Run; expect FAIL.**
- [ ] **Step 3: Implement** with `kept.or(routed)` so a retry keeps its
  environment (passing the route as `pinned` would override it).
- [ ] **Step 4: Run; expect PASS.**
- [ ] **Step 5: Commit** `Let the routing playbook choose the environment`.

### Task 15: Environments read and the dashboard capability

**Files:**
- Create: `GET /api/environments` handler (owner), capability `environments.read`
- Modify: the work read to include the attempt's environment id and version
- Test: `it_dashboard` (module `dashboard_environments`)

**Interfaces:**
- Produces `{"environments": [{"id", "project", "versions": [{"version", "digest", "provides", "toolchain", "admitted_at"}], "builds": [{"job_id", "kind", "state", "outcome", "failure"}], "ready_nodes": [node_id]}]}`
  and `work.environment: {"id", "version"} | null`.

- [ ] **Steps 1–5:** tests `environments_read_lists_versions_builds_and_ready_nodes`
  and `work_read_names_the_attempts_environment`; implement; run; commit
  `Show environments to the owner`. Open the aq-server pull request for
  Tasks 10–15.

### Task 16: The supervisor's toolchain setup task (spec step 8)

**Files:**
- Modify: `crates/aq-supervisor-host/src/service/plan.rs:47-75` (full-access
  instructions, under `supervisor_full_access` and `project_environments`)
- Test: the existing plan tests in that crate

- [ ] **Step 1: Write the failing test** `full_access_instructions_describe_the_toolchain_setup_task`
  asserting the instructions name `.aq/environments/<name>.md`,
  `default: true`, `aq environment try`, and that the base environment is in
  the binding's `allowed_environments` before the recipe merges.
- [ ] **Step 2–4:** run (FAIL), write the guidance from spec §6.8, run (PASS).
- [ ] **Step 5: Commit** `Tell the global supervisor to set up a new project's toolchain`.

### Task 17: Warm commands come from the recipe (spec step 10; aq-worker, in the Phase 1 worktree)

**Files:**
- Modify: `aq-worker` `aq-sandbox-archive/src/build_cache.rs` (`ProjectCache.warm`
  `:91`, `:567`, `:617-620`, `WarmJob::body` `:664-690`) to take `warm` from
  the admitted environment (Task 7) when the node config has none
- Test: `aq-sandbox-archive` build-cache unit tests

- [ ] **Steps 1–5:** test `warm_commands_come_from_the_admitted_environment`
  and `node_config_warm_still_wins_while_set`; implement; run; commit
  `Take build-cache warm commands from the environment`.

---

## Phase 5: aq-dashboard

Worktree `wt/env-dashboard`, branch `feat/environments`. `pnpm test`,
`pnpm test:e2e`, `pnpm typecheck`.

### Task 18: Settings › Environments and task needs

**Files:**
- Create: `src/api/adapters/environments.ts` (+ `.test.ts`),
  `src/features/settings/Environments.tsx` (+ `.test.tsx`),
  `src/test/environments.ts`
- Modify: `src/features/settings/index.tsx` (`TABS` `:23-29`, read-only branch),
  `src/app/compatibility.ts:17-28`, `src/mocks/handlers.ts:81`,
  `src/mocks/scenarios.ts:42-79`, `tests/settings-panels.spec.ts:10` (tab count
  10 → 11), `src/features/work/taskWork.ts:119-158` (optional `needs` and
  `environment`), `src/features/work/TaskStatus.tsx:113-126`, fixtures
  `src/mocks/humanGraph.ts:189`, `src/mocks/swarm.ts:191`

**Interfaces:**
- Consumes: Task 15's read and capability. Pattern: `ProjectBindings.tsx`.

- [ ] **Step 1: Write the failing tests:** `adaptEnvironments` rejects a
  malformed payload with `ContractError`; the tab renders versions, `provides`,
  the latest build's outcome and failure step, and ready nodes; with
  `environments.read` disabled it shows the reason and makes no request; the
  task status shows needs and the environment, and an older server without
  `needs` still renders.
- [ ] **Step 2–4:** run (FAIL), implement, run `pnpm test && pnpm test:e2e && pnpm typecheck` (PASS).
- [ ] **Step 5: Commit and open the aq-dashboard pull request.**

---

## Phase 6: Pilot and rollout (operations, no new code)

### Task 19: Turn it on

- [ ] **Step 1:** On this host, create the builder
  (`docker buildx create --name aq-templates --driver docker-container --bootstrap default`),
  write `builder.json` (role `template-build`, `base_template`,
  `target_repository` `127.0.0.1:5138/electricjack/aq-template`,
  `skopeo_image` pinned), and build and admit the AQ base from
  `packaging/sandbox` (Task 2 must be merged for the `aq` binary to pass the
  scan). Expected: an `environment_versions` row with no project.
- [ ] **Step 2:** Deploy aq-server, aq-worker and aq-playbooks with
  `project_environments` on a scratch stack; enroll this node with the
  `template-build` role and `RunEnvironments`.
- [ ] **Step 3:** Save the default routing and delivery documents (sources
  from Task 9) through Settings; confirm authoring seals them.
- [ ] **Step 4:** Pilot on the scratch project: file a toolchain task, watch
  `aq environment try`, the build at delivery, the merge, the admit, the node's
  ready report, and the next task placed in the new environment.
- [ ] **Step 5:** aq-dashboard: file "Set up the sandbox toolchain" for
  `aq-dashboard.web` (Node 24, pnpm 10.34.5, Playwright 1.64.1 Chromium with
  `--with-deps`); confirm `pnpm test:e2e` passes in `aq environment try`.
- [ ] **Step 6:** The Rust projects: one recipe each (`<project>.rust`), the
  tools today's template has on top of the base.
- [ ] **Step 7:** When every AQ project has an admitted environment, turn the
  feature on for the live stack, remove the `aq-template-unadmitted` pins from
  the node's `native.json`, and record the result in spec 0017.
- [ ] **Step 8:** After the owner confirms the epic, remove the
  `project_environments` feature in each repository (spec 0020).
