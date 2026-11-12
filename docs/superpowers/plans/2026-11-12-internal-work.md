# Internal Work Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One shared service through which any AQ component gets a piece of work done by a model, served by an API call or by a logged-in Codex or Claude on the server, with one place to configure it and one failure path.

**Architecture:** A new crate, `aq-internal-work`, owns the request types, a PostgreSQL queue of internal tasks, the Settings document and candidate resolution. API candidates are run by a loop inside aq-server. Login candidates are run by a new `internal` mode of the `aq-supervisor-host` binary, which claims rows from the same queue. Callers only see `submit` and `wait`.

**Tech Stack:** Rust (axum, sqlx/PostgreSQL, tokio, reqwest 0.12), `jsonschema` (new dependency), TypeScript/React (aq-dashboard, TanStack Query, Playwright).

**Spec:** [`docs/specs/0012-internal-work.md`](../../specs/0012-internal-work.md). Read it before any task.

**Branches:** `feat/internal-work` in aq-server (stacked on #131) and aq-dashboard (stacked on #105). Tasks 1 to 6 and 8 to 14 are aq-server; tasks 7 and the dashboard halves of 12 and 14 are aq-dashboard.

## Global Constraints

- New model use by an AQ component goes through this service. Do not add a provider client anywhere else.
- Internal tasks never take a worker slot and are never counted in `swarm_supervisor_admissions`, the per-provider agent limits, or any project queue. Do not call `aq_swarm_settings::admission` for them.
- The swarm availability switch and reduced-usage mode do not stop internal work.
- No restriction scheme for internal work beyond what this plan states. Do not add scopes, allowlists or sandboxes the spec does not ask for.
- A task with no workspace gets one completion and no tool loop.
- Purposes are exactly: `routing_classification`, `playbook_compilation`, `playbook_call`, `decision_revision`, `result_processing`. Unknown purposes are refused.
- Failure reasons are exactly: `provider_error`, `provider_unavailable`, `deadline_exceeded`, `budget_exceeded`, `invalid_output`, `no_candidate`.
- Usage is shared with project work: no separate budget. Record per-run usage where the provider reports it.
- Follow `AGENTS.md` build health rules: every `cargo nextest`/`cargo test` under `timeout 900`; new integration tests are modules of an existing `it_*` binary; `python3 scripts/build-budget.py` must pass. The aq-server lib line ratchet is 108842, which is why the service is its own crate. A new crate must not add a `tests/*.rs` file (test binary budget is 81).
- Migrations: the next free number on the branch is 0119. Before pushing, re-check with `git fetch origin && git ls-tree --name-only origin/feat/swarm-concurrency-controls crates/aq-server/migrations/ | tail -1` and renumber if it moved. Each migration ends with `UPDATE meta SET value='N' WHERE key='state_format_version';`.
- Every new route needs an entry in `protocol/commands/v1/routes.json`, and `crates/aq-internal-work/src` must be added to the scanned directories in `crates/aq-server/tests/it_server/commands.rs` (around line 112).
- Secrets and login material never appear in a row, a log line, an answer or an error. Read them only inside the function that uses them.

## Review Focus

1. **A host dies mid-run.** A row left `running` past its deadline must become `failed/deadline_exceeded` and be retried on the next candidate, not sit forever. Pinned in Task 5.
2. **Two runners claim the same task.** The in-server loop and one or more hosts poll the same table. A task must run once. Pinned in Task 1.
3. **A model returns prose, oversized output or JSON that does not fit the shape.** This is `invalid_output`, the next candidate is tried, and the caller never sees the bad answer. Pinned in Task 4.
4. **Settings point at a profile that was deleted, or has no candidate with a usable credential.** The request fails `no_candidate` with the profile named; it does not fall back silently to some other profile. Pinned in Task 3.
5. **Two processes rewrite the same Codex login file.** `subscription::native` copies a refreshed `auth.json` back over the credential's `secret_file`; an internal run and a supervisor turn can do this at the same moment. Pinned in Task 8.

---

## File Structure

```
crates/aq-internal-work/            new crate, no tests/ directory
  Cargo.toml
  src/lib.rs                        Purpose, Request, AnswerShape, Outcome, Failure; re-exports
  src/store.rs                      internal_tasks: submit, claim, complete, fail_candidate, sweep, get, list
  src/settings.rs                   internal_work_settings document: read, save, operation
  src/resolve.rs                    purpose -> ordered Vec<Candidate>
  src/validate.rs                   answer against AnswerShape
  src/api_backend.rs                OpenAI Responses-compatible client (from PR #64)
  src/http.rs                       settings routes, runs routes
crates/aq-server/migrations/0119_internal_work.sql
crates/aq-server/src/internal_work.rs          AppState wiring, in-server API runner loop, route mounting
crates/aq-server/src/playbooks/internal_judgments.rs   JudgmentProvider over the service
crates/aq-supervisor-host/src/internal.rs      `internal` mode: claim loop
crates/aq-supervisor-host/src/internal/codex.rs
crates/aq-supervisor-host/src/internal/claude.rs
crates/aq-supervisor-control/src/admin.rs      supervisor write gate (scope `settings`)
crates/aq-server/tests/it_access/internal_work.rs      store, resolve, service, backends (module of it_access)
crates/aq-server/tests/it_profiled/internal_work_settings.rs
aq-dashboard/src/api/internalWork.ts
aq-dashboard/src/features/settings/InternalWorkSettings.tsx
aq-dashboard/src/features/internal/InternalRun.tsx
```

---

# Part I1: the service, the queue, the API way, Settings

### Task 1: Crate, types and the internal task queue

**Files:**
- Create: `crates/aq-internal-work/Cargo.toml`, `src/lib.rs`, `src/store.rs`
- Create: `crates/aq-server/migrations/0119_internal_work.sql`
- Modify: root `Cargo.toml:3` (add `"crates/aq-internal-work"` to `members`), `crates/aq-server/Cargo.toml` (path dependency)
- Test: `crates/aq-server/tests/it_access/internal_work.rs` (add `mod internal_work;` to `tests/it_access/main.rs`)

**Interfaces:**
- Produces (`aq_internal_work`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose { RoutingClassification, PlaybookCompilation, PlaybookCall, DecisionRevision, ResultProcessing }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerShape {
    Choice { choices: Vec<String>, rationale_max_bytes: u64 },
    Json { schema: serde_json::Value },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Affected { pub kind: String, pub id: String, pub project_id: Option<Uuid> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub purpose: Purpose,
    pub instructions: String,       // at most 256 KiB
    pub input: String,              // at most 1 MiB
    pub answer: AnswerShape,
    pub blocking: bool,             // something is waiting on the answer
    pub affected: Affected,
    pub deadline_ms: u64,           // from submission; 1_000..=900_000
    pub max_output_bytes: u64,      // 1..=1_048_576
    pub escalate: bool,             // false when the caller has its own safe default (spec §8 step 1)
    pub profile_override: Option<String>,  // intelligence profile id; None uses Settings
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason { ProviderError, ProviderUnavailable, DeadlineExceeded, BudgetExceeded, InvalidOutput, NoCandidate }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt { pub provider: String, pub model: String, pub way: Way, pub reason: Option<Reason>, pub detail: String, pub elapsed_ms: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Way { Api, Login }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Answered { answer: serde_json::Value, provider: String, model: String, way: Way, elapsed_ms: u64, usage: Option<Usage>, attempts: Vec<Attempt> },
    Failed { reason: Reason, detail: String, attempts: Vec<Attempt> },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage { pub input_tokens: u64, pub output_tokens: u64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate { pub provider: String, pub model: String, pub reasoning: String, pub harness: Option<String>, pub way: Way, pub credential_index: usize }

pub struct Claimed { pub id: Uuid, pub request: Request, pub candidate: Candidate, pub candidate_index: u32, pub deadline_at_ms: i64 }

// store.rs
pub async fn submit(pool: &PgPool, request: &Request, candidates: &[Candidate], now_ms: i64) -> Result<Uuid>;
pub async fn claim(pool: &PgPool, way: Way, harnesses: &[&str], runner: &str, now_ms: i64) -> Result<Option<Claimed>>;
pub async fn complete(pool: &PgPool, id: Uuid, candidate_index: u32, answer: &serde_json::Value, usage: Option<&Usage>, now_ms: i64) -> Result<()>;
pub async fn fail_candidate(pool: &PgPool, id: Uuid, candidate_index: u32, reason: Reason, detail: &str, now_ms: i64) -> Result<()>;
pub async fn sweep(pool: &PgPool, now_ms: i64) -> Result<u64>;
pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Row>>;   // Row: every column, plus `outcome() -> Option<Outcome>`
```

- Table (migration 0119): `internal_tasks(id UUID PK, purpose TEXT, workspace_kind TEXT NOT NULL DEFAULT 'none' CHECK(workspace_kind IN ('none')), request JSONB, candidates JSONB, candidate_index INT NOT NULL DEFAULT 0, status TEXT CHECK(status IN ('queued','running','answered','failed')), blocking BOOLEAN, runner TEXT, claimed_at_ms BIGINT, deadline_at_ms BIGINT, created_at_ms BIGINT, finished_at_ms BIGINT, attempts JSONB NOT NULL DEFAULT '[]', answer JSONB, usage JSONB, reason TEXT, detail TEXT, affected_kind TEXT, affected_id TEXT, project_id UUID, escalate BOOLEAN, escalation TEXT NOT NULL DEFAULT 'none' CHECK(escalation IN ('none','supervisor','owner','resolved')), supervisor_attempts INT NOT NULL DEFAULT 0, diagnosis TEXT, retry_of UUID)`. Index on `(status, blocking DESC, created_at_ms)` and on `(affected_kind, affected_id)`.
- Behavior the tests pin:
  - `claim` takes the oldest `queued` row whose current candidate has the given `way` (and, for `Way::Login`, a harness in `harnesses`), `blocking` rows first, using `FOR UPDATE SKIP LOCKED`, and sets `running`, `runner`, `claimed_at_ms`.
  - `fail_candidate` appends an `Attempt`. For `ProviderError`, `ProviderUnavailable`, `InvalidOutput` and a per-candidate `DeadlineExceeded`, it advances `candidate_index` and returns the row to `queued` if another candidate exists and the task deadline has not passed; otherwise the row becomes `failed` with that reason. `BudgetExceeded` and `NoCandidate` fail the task at once.
  - `complete` and `fail_candidate` do nothing when `candidate_index` no longer matches or the row is not `running` (a late runner cannot overwrite).
  - `submit` with an empty `candidates` inserts the row already `failed/no_candidate`.

- [ ] **Step 1: Write the failing tests** in `internal_work.rs`, using `common::TestDb` as the neighbouring `it_access` modules do:

```rust
#[tokio::test] async fn blocking_tasks_are_claimed_before_older_background_tasks()   // submit background then blocking; claim returns the blocking id
#[tokio::test] async fn a_task_is_claimed_once_by_concurrent_runners()               // 8 concurrent claim() on one row: exactly one Some
#[tokio::test] async fn failed_candidate_requeues_on_the_next_and_records_the_attempt()
#[tokio::test] async fn last_candidate_failure_fails_the_task_with_all_attempts()
#[tokio::test] async fn late_completion_from_a_superseded_candidate_is_ignored()
#[tokio::test] async fn login_claim_skips_tasks_whose_candidate_is_api_or_another_harness()
#[tokio::test] async fn submit_without_candidates_is_failed_no_candidate()
#[tokio::test] async fn request_bounds_are_enforced()   // deadline 0, 2 MiB input, unknown purpose JSON -> Err
```

- [ ] **Step 2: Run to verify they fail**

Run: `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^internal_work::/)'`
Expected: compile error, `aq_internal_work` not found.

- [ ] **Step 3: Implement** the crate, migration and store. `Request::validate(&self) -> Result<()>` enforces the bounds in the comments above; `submit` calls it.

- [ ] **Step 4: Run to verify they pass**

Run: same command. Expected: 8 passed. Then `python3 scripts/build-budget.py` passes.

- [ ] **Step 5: Commit** `feat(internal-work): task queue and request types`

### Task 2: Settings document (default profile and overrides)

**Files:**
- Create: `crates/aq-internal-work/src/settings.rs`, `src/http.rs`
- Modify: `0119_internal_work.sql` (add the two tables), `crates/aq-server/src/internal_work.rs` (new; mounts routes), `crates/aq-server/src/lib.rs` (merge into the admin router beside `profile_catalog::admin_routes()` at ~317 so `csrf_guard` covers it), `protocol/commands/v1/routes.json`, `tests/it_server/commands.rs:112-117`
- Test: `crates/aq-server/tests/it_profiled/internal_work_settings.rs` (add to `tests/it_profiled/main.rs`)

**Interfaces:**
- Consumes: the pattern in `crates/aq-profile-settings/src/structure.rs` (`read`, `operation`, `save`, replay by `(actor, operation_id)` and payload digest, revision fence, `FOR SHARE` on `profile_catalog_registry`). Copy its structure; do not share code with it.
- Produces:

```rust
pub struct Document { pub default_profile_id: Option<String>, pub overrides: BTreeMap<Purpose, String> }
pub struct SaveRequest { pub operation_id: Uuid, pub expected_revision: String, pub expected_catalog_revision: String, #[serde(flatten)] pub document: Document }
pub async fn read(pool: &PgPool) -> Result<Value>;      // {revision, catalog_revision, default_profile_id, overrides, purposes:[...all five...]}
pub async fn save(pool: &PgPool, actor: Uuid, req: SaveRequest) -> Result<Value>;
pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid) -> Result<Value>;
pub async fn profile_for(pool: &PgPool, purpose: Purpose) -> Result<Option<String>>;  // override, else default
```

- Tables: `internal_work_settings(singleton BOOLEAN PK DEFAULT TRUE CHECK(singleton), revision BIGINT NOT NULL DEFAULT 0, document JSONB NOT NULL)` seeded `{"default_profile_id":null,"overrides":{}}`, and `internal_work_settings_operations` with the same columns as `intelligence_structure_operations`.
- Routes, both `RequireOwner`, class `owner_admin` in `routes.json`: `GET|POST /api/settings/internal-work`, `GET /api/settings/internal-work/operations/{id}`.
- `save` refuses (400) a profile id that is not in the active catalog, or that is an `automatic_routing` profile.

- [ ] **Step 1: Write the failing tests**, reusing `settings_fixture()` and `call(...)` from `it_profiled/profile_catalog.rs` (make them `pub(crate)` there):

```rust
#[tokio::test] async fn default_and_override_round_trip_and_replay_returns_the_same_receipt()
#[tokio::test] async fn stale_revision_and_stale_catalog_revision_conflict()            // 409
#[tokio::test] async fn unknown_profile_and_automatic_router_are_refused()               // 400
#[tokio::test] async fn unknown_purpose_key_is_refused()                                 // 400
#[tokio::test] async fn save_requires_owner_and_csrf_header()                            // 403 without X-AQ-Request: 1
#[tokio::test] async fn profile_for_uses_override_then_default_then_none()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^internal_work_settings::/)'`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass**, then `timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/every_mounted_route_is_a_command_or_reviewed/)'` passes.
- [ ] **Step 5: Commit** `feat(internal-work): settings default and per-purpose overrides`

### Task 3: Candidate resolution

**Files:**
- Create: `crates/aq-internal-work/src/resolve.rs`
- Test: `it_access/internal_work.rs`

**Interfaces:**
- Consumes: `aq_profiles::{ValidatedCatalog, Intelligence, ModelCandidate}`; `aq_intelligence_config::LoadedRegistry` (`credentials: Vec<AuthorizedCredential>`, `config.credentials[i].kind: CredentialKind`); `AvailabilityObservation` rows from `intelligence_observations` (read as `resolver.rs:148` does).
- Produces:

```rust
pub struct Resolved { pub profile_id: String, pub profile_version: u32, pub candidates: Vec<Candidate> }
pub fn resolve(catalog: &ValidatedCatalog, registry: &LoadedRegistry, observations: &[AvailabilityObservation],
               profile_id: &str, now_ms: u64) -> Result<Resolved, Reason>;
```

- Rules: take the profile's candidates in catalog order. For each, find the first enabled credential whose `provider` matches. `CredentialKind::ApiKey` gives `Way::Api`; `SubscriptionLogin` gives `Way::Login` with `harness` set to the candidate's first harness that is `codex` or `claude`. Drop a candidate with no credential, a login candidate with no such harness, and a candidate whose credential has an unexpired observation in state `QuotaUnavailable`, `AuthenticationFailed` or `TemporaryUnavailable` (cooldown not passed). A profile that is missing, or left with no candidates, is `Err(Reason::NoCandidate)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn candidates_keep_profile_order_and_carry_the_way_of_their_credential()
#[test] fn quota_unavailable_login_is_skipped_so_the_next_provider_is_first()     // the owner's "fall back to another provider"
#[test] fn expired_observation_does_not_skip()
#[test] fn missing_profile_and_profile_without_usable_credentials_are_no_candidate()
#[test] fn login_candidate_whose_only_harness_is_gemini_is_dropped()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(internal-work): resolve a purpose to ordered candidates`

### Task 4: Answer validation and the API backend

**Files:**
- Create: `crates/aq-internal-work/src/validate.rs`, `src/api_backend.rs`
- Modify: `crates/aq-internal-work/Cargo.toml` (add `jsonschema` with default features off, `reqwest.workspace = true`, `zeroize = "1"`), `crates/aq-server/src/config.rs` (new optional `[internal_work]` section)
- Test: `it_access/internal_work.rs`

**Interfaces:**
- Consumes: PR #64's `ResponsesJudgmentProvider` (`gh pr diff 64`): request body, `output_text` and `usage` parsing, the `O_NOFOLLOW` owner-only secret read, `reqwest::Client` built with `redirect(Policy::none()).no_proxy()`. Port the request and secret code; drop its `PermittedScope`, pricing and `JudgmentProvider` impl.
- Produces:

```rust
// validate.rs
pub fn prompt(request: &Request) -> String;   // instructions, then input, then the exact answer contract below
pub fn answer(shape: &AnswerShape, raw: &str, max_output_bytes: u64) -> Result<serde_json::Value, String>;
// api_backend.rs
#[derive(Deserialize)] #[serde(deny_unknown_fields)]
pub struct ApiEndpoint { pub provider: String, pub endpoint: String, pub secret_file: PathBuf }
pub struct ApiBackend { /* endpoints by provider, client */ }
impl ApiBackend {
    pub fn new(endpoints: Vec<ApiEndpoint>) -> Result<Self>;
    pub async fn run(&self, claimed: &Claimed) -> Result<(String, Option<Usage>), (Reason, String)>;  // raw model text
}
```

- Config: `[internal_work] api = [{provider="openai", endpoint="https://…/v1/responses", secret_file="/abs/path"}]`, `poll_ms = 200` (default), `enabled = true` (default).
- Answer contract appended by `prompt`: for `Choice`, "Answer with one JSON object and nothing else: {"choice": one of <choices>, "rationale": string}". For `Json`, "Answer with one JSON object and nothing else, matching this JSON Schema:" followed by the schema.
- `answer`: reject raw longer than `max_output_bytes`; strip one surrounding Markdown code fence if present; parse as JSON; for `Choice` require `choice` in `choices` and `rationale` within `rationale_max_bytes`; for `Json` validate with `jsonschema`. The error string names the first violation and never includes more than 200 bytes of the model's text.
- API failure mapping: HTTP 408, 429, 5xx and transport errors are `ProviderUnavailable`; other non-2xx are `ProviderError`; a provider with no configured endpoint is `ProviderUnavailable` with detail `no api endpoint configured for <provider>`.

- [ ] **Step 1: Write the failing tests** (the API tests use a local axum fake endpoint on `127.0.0.1:0`, as #64's test does):

```rust
#[test] fn fenced_json_is_accepted_and_prose_is_invalid_output()
#[test] fn choice_outside_the_list_and_oversized_rationale_are_invalid()
#[test] fn json_answer_is_checked_against_the_schema()
#[test] fn oversized_output_is_invalid_and_the_error_does_not_echo_it()
#[tokio::test] async fn api_backend_returns_text_and_usage_and_never_stores_the_secret()   // secret absent from Row JSON and error strings
#[tokio::test] async fn api_429_and_500_are_unavailable_and_400_is_error()
#[tokio::test] async fn secret_file_that_is_group_readable_or_a_symlink_is_refused()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(internal-work): answer validation and API backend`

### Task 5: The service, the in-server runner and the sweeper

**Files:**
- Modify: `crates/aq-server/src/internal_work.rs`, `crates/aq-server/src/lib.rs` (construct, store in `AppState`, spawn the loop next to the other background tasks)
- Test: `it_access/internal_work.rs`

**Interfaces:**
- Produces (in aq-server):

```rust
#[derive(Clone)] pub struct InternalWork { /* pool, clock, api: Arc<ApiBackend>, registry, poll_ms */ }
impl InternalWork {
    pub async fn submit(&self, request: Request) -> Result<Uuid, AppError>;   // settings -> profile -> resolve -> store::submit
    pub async fn wait(&self, id: Uuid) -> Result<Outcome, AppError>;          // polls store::get every poll_ms until answered/failed
    pub async fn run(&self, request: Request) -> Result<Outcome, AppError>;   // submit + wait
    pub(crate) async fn tick(&self) -> Result<bool, AppError>;                // sweep, then claim one Way::Api task and run it
}
// AppState gains: pub internal_work: InternalWork
```

- `submit` uses `request.profile_override`, else `settings::profile_for(purpose)`. With neither set the task is `failed/no_candidate` with detail `no internal work profile configured`.
- `tick` runs one API task end to end: `ApiBackend::run`, then `validate::answer`, then `store::complete` or `store::fail_candidate`. The per-candidate HTTP timeout is the smaller of 120 s and the time left to the task deadline.
- `store::sweep`: every `running` row whose `claimed_at_ms` is older than `min(120 s, deadline)` plus 5 s grace gets `fail_candidate(DeadlineExceeded, "runner did not report")`. Every `queued` row past `deadline_at_ms` becomes `failed/deadline_exceeded`.
- The loop calls `tick` until it returns false, then sleeps `poll_ms`. It runs regardless of swarm availability.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn run_returns_the_validated_answer_from_the_first_candidate()
#[tokio::test] async fn invalid_output_from_the_first_candidate_falls_to_the_second()
#[tokio::test] async fn no_configured_profile_fails_no_candidate_naming_the_setting()
#[tokio::test] async fn a_running_task_whose_runner_vanished_is_swept_and_retried_on_the_next_candidate()   // set claimed_at_ms in the past, tick
#[tokio::test] async fn a_queued_task_past_its_deadline_fails_deadline_exceeded()
#[tokio::test] async fn internal_work_runs_while_swarm_availability_is_off_and_adds_no_swarm_admission_rows()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(internal-work): service, API runner loop and sweeper`

### Task 6: Serve `judgment.request` through the service

**Files:**
- Create: `crates/aq-server/src/playbooks/internal_judgments.rs`
- Modify: `crates/aq-server/src/playbooks/runtime.rs:135` (pass `Some(Arc::new(InternalJudgments::new(state.internal_work.clone(), Purpose::PlaybookCall)))` instead of `None`; update the comment above it), `crates/aq-playbook-host/README.md` (the sentence saying `judgment.request` refuses)
- Test: `crates/aq-server/tests/it_playbook/playbook_host_requests.rs`

**Interfaces:**
- Consumes: `JudgmentProvider`, `JudgmentRequest`, `JudgmentReply`, `ProviderFailure` (`playbooks/host_adapters/requests.rs:120-180`); `InternalWork::run`.
- Produces: `pub struct InternalJudgments { work: InternalWork, purpose: Purpose }` implementing `JudgmentProvider`.
  - `credential_profile` returns `Some("internal-work".into())` for every workspace and project.
  - `complete` submits `Request { purpose, instructions: question, input: context_text, answer: Choice { choices, rationale_max_bytes }, blocking: true, affected: Affected { kind: "playbook_operation", id: operation }, deadline_ms, max_output_bytes: rationale_max_bytes + 4096, escalate: false, profile_override: None }`.
  - `Outcome::Answered` maps to `JudgmentReply` with `cost_microunits: 0` and token counts from `usage` or the request's estimate. `Failed` with `ProviderUnavailable`, `DeadlineExceeded` or `NoCandidate` maps to `ProviderFailure::Unavailable`; every other reason maps to `ProviderFailure::Error`.

- [ ] **Step 1: Write the failing test** beside `judgment_request_respects_budget_and_records_provenance`:

```rust
#[tokio::test] async fn judgment_request_is_served_by_internal_work_and_records_its_provider_and_model()
// host built with InternalJudgments over an InternalWork whose API backend is the local fake endpoint;
// assert the completed operation's provenance provider/model equal the fake's, and one internal_tasks row
// exists with purpose 'playbook_call' and affected_kind 'playbook_operation'.
#[tokio::test] async fn judgment_request_with_no_internal_profile_fails_provider_unavailable()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo nextest run -p aq-server --test it_playbook -E 'test(/^playbook_host_requests::judgment/)'`
- [ ] **Step 3: Implement.** **Step 4: Run to verify they pass**, including the existing `FakeJudge` tests and `real_adapter_pilots::pilots_end_to_end`.
- [ ] **Step 5: Commit** `feat(playbooks): serve judgment.request through internal work`

### Task 7: Settings panel (aq-dashboard)

**Files:**
- Create: `src/api/internalWork.ts`, `src/api/internalWork.test.ts`, `src/features/settings/InternalWorkSettings.tsx`, `InternalWorkSettings.test.tsx`
- Modify: `src/features/settings/intelligence.tsx` (render `<InternalWorkSettings />` directly above `<RoutingPolicyEditor />`, ~line 340)
- Test: `tests/intelligence-structure.spec.ts` (add one case)

**Interfaces:**
- Consumes: `GET|POST /api/settings/internal-work`, `GET …/operations/{id}`; the client pattern of `src/api/intelligenceStructure.ts`; the profile list already loaded on the Intelligence tab.
- Produces: `readInternalWorkSettings(signal?)`, `saveInternalWorkSettings(request)`, `inspectInternalWorkSave(operationId)`; `export function InternalWorkSettings()` (no props).
- UI: heading "Internal work". One select "Default profile" and one select per purpose, labelled "Routing classification", "Playbook compilation", "Playbook calls", "Decision revision", "Result processing", each with a first option "Use default". Automatic-router profiles are not offered. One Save button; a 409 shows "Settings changed elsewhere. Reload to continue." and keeps the draft. With no default chosen, show "No profile is set. Internal work will fail until one is chosen."

- [ ] **Step 1: Write the failing tests**: unit tests for load, save payload (`operation_id`, both expected revisions, only non-default overrides sent), conflict message, the empty-default warning; Playwright: choose a default, set the playbook compilation override, save, reload, both persist.
- [ ] **Step 2: Run to verify they fail**: `pnpm test -- InternalWorkSettings internalWork` and `pnpm test:e2e -- intelligence-structure`
- [ ] **Step 3: Implement.** **Step 4: Run to verify they pass**, then `pnpm typecheck && pnpm lint`.
- [ ] **Step 5: Commit** `feat(settings): internal work default and overrides`

---

# Part I2: logged-in Codex and Claude on the server

### Task 8: `internal` mode of the supervisor host, with the Codex runner

**Files:**
- Create: `crates/aq-supervisor-host/src/internal.rs`, `src/internal/codex.rs`
- Modify: `crates/aq-supervisor-host/src/main.rs:54,87` (accept `internal CONFIG`), `src/lib.rs`, `Cargo.toml` (path dependency on `aq-internal-work`)
- Test: `crates/aq-supervisor-host/tests/adapter.rs` (add a module; do not add a test file)

**Interfaces:**
- Consumes: `store::{claim, complete, fail_candidate}`, `validate::{prompt, answer}`; the command construction, executable digest check, uid drop and `auth.json` handling in `src/subscription.rs:16-146`. Extract the shared parts of `native` into private helpers used by both; do not duplicate them.
- Produces:

```rust
#[derive(Deserialize)] #[serde(deny_unknown_fields)]
pub struct InternalConfig {
    pub database_url: String, pub homes: PathBuf, pub scratch: PathBuf, pub poll_ms: u64,
    pub codex: Option<Harness>, pub claude: Option<Harness>,
}
#[derive(Deserialize)] #[serde(deny_unknown_fields)]
pub struct Harness { pub native: PathBuf, pub native_digest: Digest, pub secret_file: PathBuf }
pub async fn run(config: InternalConfig) -> Result<()>;      // claim loop; never returns unless the config file changes or is removed
pub(crate) async fn codex(h: &Harness, homes: &Path, scratch: &Path, claimed: &Claimed) -> Result<(String, Option<Usage>), (Reason, String)>;
```

- The loop claims with `Way::Login` and the harness names that are configured, runner name `internal-host:<hostname>`. It runs one task at a time per harness.
- Codex invocation: the arguments of `subscription.rs:73-133` with these differences: `--sandbox read-only`; `--cd` a fresh empty directory under `scratch`, removed afterwards; no `developer_instructions`, no `AQ_SUPERVISOR_*` variables; `model` and `model_reasoning_effort` from the candidate; the prompt on stdin is `validate::prompt(&request)`. Output is read from `--output-last-message` as `native` does.
- **Deviation the spec's owner must know:** the Codex CLI has no switch that removes its shell tool. "Tools off" for Codex is a read-only sandbox in an empty directory with shell environment `inherit="none"`. Record this sentence in `docs/implementation/0012-internal-work.md` when the task lands.
- Login file: take an exclusive `flock` on `<secret_file>.lock` around copying `auth.json` in and copying a refreshed one back, and change `subscription::native` to take the same lock, so an internal run and a supervisor turn cannot interleave writes.
- Failure mapping: exit status non-zero with an authentication or usage-limit message in the JSON event stream is `ProviderUnavailable`; a kill at the timeout is `DeadlineExceeded`; any other non-zero exit is `ProviderError`. `detail` is the last error event's message, redacted with the existing token redaction, at most 500 bytes.

- [ ] **Step 1: Write the failing tests.** Use a fake native executable (a shell script written by the test, with its digest computed in the test) that records its argv and stdin and writes `--output-last-message`:

```rust
#[tokio::test] async fn codex_run_passes_read_only_sandbox_an_empty_cwd_and_the_validated_prompt()
#[tokio::test] async fn codex_answer_is_completed_on_the_task_and_scratch_is_removed()
#[tokio::test] async fn codex_usage_limit_is_provider_unavailable_and_the_task_requeues_on_the_next_candidate()
#[tokio::test] async fn two_concurrent_runs_never_interleave_auth_json_writes()      // Review Focus 5: fake native sleeps and rewrites auth.json; file is always one writer's whole content
#[tokio::test] async fn native_whose_digest_differs_is_refused_before_exec()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo test -p aq-supervisor-host --test adapter -- internal::`
- [ ] **Step 3: Implement.** **Step 4: Run to verify they pass**, plus the existing `adapter` and `supervisor_session` tests.
- [ ] **Step 5: Commit** `feat(supervisor-host): internal mode with the Codex runner`

### Task 9: The Claude runner

**Files:**
- Create: `crates/aq-supervisor-host/src/internal/claude.rs`
- Modify: `src/internal.rs`
- Test: `tests/adapter.rs` (same module)

**Interfaces:**
- Consumes: the Claude invocation in aq-worker `crates/aq-worker/src/native/adapter/claude.rs:122-157` (arguments and environment) and the result parsing in aq-worker `crates/aq-handoff-note/src/stream.rs`. Copy what is needed; aq-server does not depend on aq-worker.
- Produces: `pub(crate) async fn claude(h: &Harness, homes: &Path, scratch: &Path, claimed: &Claimed) -> Result<(String, Option<Usage>), (Reason, String)>;`
- Invocation: `-p --output-format json --input-format text --model <candidate.model> --setting-sources user --settings <home>/settings.json --strict-mcp-config --no-session-persistence --disable-slash-commands`, prompt on stdin, cwd a fresh empty scratch directory, environment exactly `HOME`, `CLAUDE_CONFIG_DIR` (a private per-run home), `PATH`, `DISABLE_AUTOUPDATER=1`, `DISABLE_TELEMETRY=1`, `DISABLE_ERROR_REPORTING=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`, and `CLAUDE_CODE_OAUTH_TOKEN` read from `secret_file` (normalised with `aq_intelligence_config::material::normalize_claude_token`).
- Tools off: before writing the code, run the pinned CLI's `--help` (the executable the local stack mounts for workers) and use its tool-disabling option. Prefer `--tools ""` if listed; otherwise deny every built-in tool in the generated `settings.json` `permissions.deny`. Record which was used, with the CLI version, in `docs/implementation/0012-internal-work.md`.
- Result: the JSON object's `result` string is the raw answer; `usage.input_tokens`/`output_tokens` fill `Usage`; `is_error: true` or a non-zero exit is a failure, mapped as in Task 8 (authentication and usage-limit text is `ProviderUnavailable`).
- Same uid drop, digest check, timeout and redaction as Codex.

- [ ] **Step 1: Write the failing tests** with a fake native:

```rust
#[tokio::test] async fn claude_run_passes_the_token_only_in_the_environment_and_never_in_argv_or_rows()
#[tokio::test] async fn claude_result_and_usage_complete_the_task()
#[tokio::test] async fn claude_is_error_with_usage_limit_text_is_provider_unavailable()
#[tokio::test] async fn a_profile_listing_codex_then_claude_falls_back_to_claude_when_codex_is_unavailable()   // spec: fallback between providers is real
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(supervisor-host): Claude runner for internal work`

### Task 10: Packaging and the local stack

**Files:**
- Modify: `compose.workers.yaml` (service `internal-work`, same image as the supervisor host, command `internal /config/internal.json`, read-only mounts for the two native executables and the two secret files, a private volume for `homes` and `scratch`, own PID namespace, no Docker socket), `compose.dev.yaml`, `scripts/dev-stack.py` (generate `internal.json`)
- Create: `docs/operations/internal-work.md`, `docs/implementation/0012-internal-work.md`
- Test: `crates/aq-server/tests/it_server/config.rs` (the `[internal_work]` section parses and rejects unknown keys)

- [ ] **Step 1: Write the failing config test.** **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement** the compose service, the dev-stack generation and the operations page (what to mount, how to tell the host is polling: a `running` row's `runner` column, and the log line `internal host claimed <id>`).
- [ ] **Step 4: Verify** `docker compose -f compose.yaml -f compose.workers.yaml config` succeeds and the config test passes.
- [ ] **Step 5: Commit** `feat(deploy): internal work host service`
- [ ] **Step 6: Live check on `aq-ms1` (owner approves the rollout first).** With the default profile set to `routing-sol-low`, submit one `playbook_call` choice request through a playbook judgment or `psql` insert, and record in `docs/implementation/0012-internal-work.md`: the row's provider, model, way `login`, elapsed time, and that no `swarm_supervisor_admissions` row was added. Then save a test profile whose first candidate is a provider with no credential on this stack and whose second is Claude, set it as the `playbook_call` override, and record a run answered by Claude with the skipped candidate absent from `candidates`. Restore the override afterwards.

---

# Part I3: seeing it, and the failure path

### Task 11: Runs API, retry, and the History filter

**Files:**
- Modify: `crates/aq-internal-work/src/http.rs`, `src/store.rs` (`list`, `retry`), `protocol/commands/v1/routes.json`
- aq-dashboard: create `src/features/internal/InternalRun.tsx` (+ test); modify `src/api/internalWork.ts`, `src/features/agents/history/History.tsx` (+ test)
- Test: `it_access/internal_work.rs`

**Interfaces:**
- Produces (server), all `RequireOwner`, class `owner_admin`:
  - `GET /api/internal-work/runs?affected_kind=&affected_id=&purpose=&status=&cursor=&limit=` returning `{runs:[Run], next_cursor}`; newest first, limit at most 100, default 50, cursor `"ms:uuid"` as the manual-agents history uses.
  - `GET /api/internal-work/runs/{id}` returning `Run`.
  - `POST /api/internal-work/runs/{id}/retry` returning `{id: <new uuid>}`. It re-submits the stored request with fresh candidate resolution and sets `retry_of`. Refused (409) unless the run is `failed`.
  - `Run`: `{id, purpose, status, affected:{kind,id,project_id}, instructions, input, answer?, reason?, detail?, attempts, provider?, model?, way?, elapsed_ms?, usage?, created_at_ms, finished_at_ms?, escalation, diagnosis?, retry_of?}`.
- Produces (dashboard): `listInternalRuns(params, signal?)`, `readInternalRun(id)`, `retryInternalRun(id)`; `export function InternalRun({affectedKind, affectedId}: {affectedKind: string; affectedId: string})`, which shows the newest run for that item: what was asked (collapsed), which model answered, how long it took, the answer or the error, each attempt, and a Retry button on a failed run. It renders nothing when there is no run.
- History: a checkbox "Internal", off by default. When on, the list is the internal runs (purpose, affected item, status, model, time), not merged with manual-agent sessions.

- [ ] **Step 1: Write the failing tests**: server `runs_are_listed_newest_first_and_filtered_by_affected_item`, `retry_resubmits_a_failed_run_and_refuses_a_running_one`, `runs_routes_require_owner`; dashboard unit tests for `InternalRun` (answered, failed with attempts, empty, retry click) and History (filter off shows sessions and makes no runs request; on shows runs).
- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass**, plus the mounted-route audit test.
- [ ] **Step 5: Commit** in each repository: `feat(internal-work): runs API, retry and History filter`

### Task 12: Supervisor authority over Settings and internal runs

**Files:**
- Create: `crates/aq-supervisor-control/src/admin.rs`
- Modify: `crates/aq-supervisor-control/src/authority.rs:62` (`SCOPES` becomes six, adding `"settings"`), `src/lib.rs`, `crates/aq-server/src/manual_agents.rs:64` (mount beside the read nest), `protocol/commands/v1/routes.json`
- Test: `crates/aq-server/tests/it_dashboard/` in the existing `supervisor_*` module that covers the read nest

**Interfaces:**
- Consumes: `read.rs:45 gate` and `:79 routes` (the pattern: authenticate the bearer for a scope, insert the extension `authz::resolve` turns into an Owner user).
- Produces: `pub fn routes<S>(routes: Router<S>, pool: PgPool, clock: Clock) -> Router<S>` mounting `/v1/supervisor/admin/*`. Its gate authenticates scope `settings`, allows GET, HEAD and POST, and forwards only paths under `/api/settings/intelligence`, `/api/settings/internal-work` and `/api/internal-work/`. Any other path is 404. The CSRF header is not required on this nest (the bearer is the authority).
- New supervisor credentials carry all six scopes. Existing credentials keep five and get 403 on the admin nest until reissued; say so in `docs/operations/internal-work.md`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn supervisor_with_settings_scope_can_save_internal_work_settings_and_the_routing_policy()
#[tokio::test] async fn supervisor_without_settings_scope_gets_403_on_the_admin_nest()
#[tokio::test] async fn admin_nest_refuses_paths_outside_settings_and_internal_work()   // e.g. /api/worker-logins -> 404
#[tokio::test] async fn supervisor_can_list_and_retry_internal_runs()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(supervisor): authority over Intelligence settings and internal runs`

### Task 13: The failure path: supervisor first, then the owner

**Files:**
- Modify: `crates/aq-internal-work/src/store.rs` (`escalate_due`, `mark_supervisor`, `mark_owner`, `resolve`), `src/http.rs`, `crates/aq-server/src/internal_work.rs` (escalation step in `tick`), `crates/aq-server/src/agent_access/messaging/global/writes.rs` (system message), `crates/aq-dashboard-contract/src/lib.rs:345` (`AttentionKind::InternalWork`), `crates/aq-server/src/dashboard_api/aggregation.rs`, `protocol/dashboard-read/v1/schema.json` (regenerate), `protocol/commands/v1/routes.json`
- aq-dashboard: `contracts/pinned/dashboard-read/v1/schema.json` (re-pin with `scripts/pin-contract.py`), `src/api/adapters/overview.ts` (+ `overviewContract.test.ts`)
- Test: `it_access/internal_work.rs`, `it_dashboard/dashboard_aggregation.rs`

**Interfaces:**
- Consumes: the insert the human send handler uses in `messaging/global/writes.rs`; `attention(kind, subject, project, title, reason, since)` at `aggregation.rs:129`; the producer shape of `offline` at `:417`.
- Produces:

```rust
// writes.rs
pub(crate) async fn system_message(pool: &PgPool, project: Option<Uuid>, body: &str, at_ms: u64) -> Result<Uuid>;
// project None addresses the global supervisor's conversation; Some addresses that project's supervisor, falling back to global when the project has none.
```

  - Routes on the admin nest's allowed prefix, owner or supervisor: `POST /api/internal-work/runs/{id}/escalate` with `{diagnosis: string}` (at most 4000 bytes), and `POST /api/internal-work/runs/{id}/resolve`.
- Rules (the numbers are this plan's choice; the spec leaves them to the builder):
  - A run that ends `failed` with `escalate = true` gets `escalation = 'supervisor'`. A purpose with 5 failed runs in 10 minutes escalates its newest failed run even when `escalate = false`, once per 10 minutes.
  - On escalation to the supervisor the server sends one `system_message` to the project's supervisor (`project_id` set) or the global one: the purpose, the affected item, the reason, each attempt's provider, model and detail, and the three things it may do: retry (`/retry`), fix the cause in Settings then retry, or `/escalate` with a diagnosis.
  - `retry` by a supervisor increments `supervisor_attempts` on the original run and carries the count to the new run. A run whose count reaches 3 and fails again goes straight to `escalation = 'owner'` with diagnosis `supervisor attempts exhausted`. `/escalate` sets `owner` with the supervisor's diagnosis. A retried run that answers sets the original's `escalation = 'resolved'`.
  - A run in `supervisor` state for 30 minutes with no retry, escalate or resolve goes to `owner` with diagnosis `supervisor did not respond`.
  - Attention: one `AttentionKind::InternalWork` item per run with `escalation = 'owner'`. `subject` is the affected item (`reference(affected_kind, affected_id, …)`), `title` is "Internal work failed", `reason` is the diagnosis, `project` from `project_id`. It disappears when the run is resolved or a retry of it answers.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn failed_run_with_escalate_messages_the_project_supervisor_once()
#[tokio::test] async fn failed_run_without_escalate_raises_nothing()
#[tokio::test] async fn five_failures_of_one_purpose_in_ten_minutes_escalate_once()
#[tokio::test] async fn third_failed_supervisor_retry_reaches_the_owner()
#[tokio::test] async fn supervisor_escalate_records_its_diagnosis_and_reaches_the_owner()
#[tokio::test] async fn silent_supervisor_reaches_the_owner_after_thirty_minutes()
#[tokio::test] async fn answered_retry_resolves_the_original_and_removes_the_attention_item()
// it_dashboard:
#[tokio::test] async fn owner_escalated_internal_run_is_an_attention_item_pointing_at_the_affected_item()
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** Regenerate the contract with `cargo run -p aq-server --example dashboard_contract --offline`; in aq-dashboard re-pin and map the new kind in `overview.ts` (label "Internal work", link to the affected item).
- [ ] **Step 4: Run to verify they pass**: the two server modules, `cargo run -p aq-server --example dashboard_contract --offline -- --check`, and in aq-dashboard `pnpm test -- overviewContract overview`.
- [ ] **Step 5: Commit** in each repository: `feat(internal-work): failure path through a supervisor to the owner`

---

## What this plan leaves out

- Internal tasks with a repository to read, decision revision and result processing (spec §9 "Later"). `workspace_kind` exists so they can be added.
- Wider supervisor authority beyond Settings and internal runs (spec §8).
- A playbook asking for a stronger or weaker level than the playbook-call default (spec §5). `Request.profile_override` carries it inside the service, but `judgment.request` has no field for it; adding one is a host catalog change and belongs with the catalog work in the routing plan.
- The inline run view on a specific page. `InternalRun` is built here; each consumer places it. The routing plan places it on the task view and the routing policy editor.
