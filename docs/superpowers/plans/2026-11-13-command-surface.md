# Command Surface (summaries, read parameters, CLI stubs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every registered command has a real summary, every generated read schema lists its query params and refuses others, and the three stubbed owner CLI commands run their registered commands.

**Architecture:** All registry changes live in `crates/aq-command-registry` (a pure crate): a summary table for action kinds and a declared query-param list on `Route`. `forward` enforces the list. `crates/aqv2` routes three stubs through `POST /api/v1/commands/{name}`. `protocol/commands/v1/commands.json` is regenerated.

**Tech Stack:** Rust, serde_json, clap, cargo nextest.

**Spec:** `docs/superpowers/specs/2026-11-13-command-surface-design.md`

## Global Constraints

- Do not add lines to the aq-server library (`crates/aq-server/src`); the line ratchet may only go down. Check with `python3 scripts/build-budget.py`.
- Do not edit any `ActionKind` implementation.
- No new test binary. New tests go into existing modules.
- Every `cargo test` or `cargo nextest` runs under `timeout 900`.
- Iterate with `cargo nextest run`, not `cargo build`. `cargo clippy` once at the end.
- Regenerate `commands.json` only with `AQ_REGENERATE_COMMANDS=1 timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`.

## Review Focus

1. A declared query list that omits a param the handler accepts: a caller that used it through the command surface is now refused. Each list is taken from the handler's `Query<T>` type, and Task 2 records the handler beside the declaration.
2. A declared param the handler does not accept: published but ignored. Same control as 1.
3. Existing callers of `aq run` or typed `aqv2` wrappers that pass a param outside the list: covered by the existing `aqv2` and `it_access::agent_commands` tests, which must pass unchanged.
4. A supervisor read route (`/v1/supervisor/read/...`) must take the same list as its owner route: Task 2 test `supervisor_reads_declare_the_owner_query`.
5. An action kind registered without a summary (the playbook branch adds three): Task 1 test fails with the kind named.

---

### Task 1: Action summaries

**Files:**
- Create: `crates/aq-command-registry/src/summaries.rs`
- Modify: `crates/aq-command-registry/src/lib.rs` (the `None =>` arm of `Registry::new`)
- Test: `crates/aq-server/tests/it_server/commands.rs`

**Interfaces:**
- Produces: `pub fn action_summary(kind: &str) -> Option<&'static str>`; `pub const PLACEHOLDER_SUMMARY: &str` (the current placeholder text).

- [ ] **Step 1: Failing test** in `commands.rs`:

```rust
#[test]
fn every_command_has_its_own_summary() {
    for c in aq_server::commands::registry().commands() {
        assert!(
            c.summary != aq_command_registry::PLACEHOLDER_SUMMARY && c.summary.len() >= 12,
            "{} has no summary; add it to crates/aq-command-registry/src/summaries.rs",
            c.name
        );
    }
}
```

- [ ] **Step 2:** Run `timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`. Expected: this test fails naming `comment.revise`.
- [ ] **Step 3:** Write `summaries.rs` with one entry per action kind that has no catalog entry (32 today). Each summary is one sentence fragment, under 100 characters, stating the effect and the subject, written from the action's payload schema and handler. `Registry::new` uses `action_summary(a.kind).unwrap_or(PLACEHOLDER_SUMMARY)`.
- [ ] **Step 4:** Run the same command. Expected: `every_command_has_its_own_summary` passes; `commands_json_is_generated_from_the_registry` fails (regenerated in Task 4).

### Task 2: Declared query params

**Files:**
- Modify: `crates/aq-command-registry/src/lib.rs` (`Route`, `params_schema`, `forward`)
- Modify: `crates/aq-command-registry/src/catalog.rs` (each `Route::get`)
- Test: `crates/aq-command-registry/src/tests.rs`

**Interfaces:**
- Produces:

```rust
pub struct QueryParam { pub name: &'static str, pub schema: &'static str, pub required: bool }
impl Route { pub const fn query(self, query: &'static [QueryParam]) -> Self; }
// Route gains: pub query: &'static [QueryParam]  (empty for every non-GET route)
```

- [ ] **Step 1: Failing tests** in `tests.rs`:
  - `generated_read_schema_is_closed`: for every catalog route with `Params::Path`, `params_schema(..)["additionalProperties"] == false`, every declared param appears in `properties` with its parsed schema, and required ones appear in `required`.
  - `forward_refuses_an_undeclared_query_param`: `task.show` with `{"project_id":p,"task_id":"t-1","bogus":1}` returns `BuildError("unknown param")`; with a declared param it builds the same URI as before.
  - `supervisor_reads_declare_the_owner_query`: for every command with both an owner and a supervisor `Body::Query` route, the two `query` slices are equal.
  - `declared_query_schemas_parse`: every `QueryParam.schema` is valid JSON with a `type` or `enum`, and names are unique per route and distinct from the path params.
- [ ] **Step 2:** Run `timeout 900 cargo nextest run -p aq-command-registry`. Expected: the new tests fail.
- [ ] **Step 3:** Add `QueryParam`, `Route.query` and `Route::query`. `params_schema` for `Params::Path` emits path params plus declared params, `additionalProperties: false`, and a description that no longer promises pass-through. `forward` for `Body::Query` returns `BuildError("unknown param")` for a remaining param not in `route.query`.
- [ ] **Step 4:** For each of the 65 routes (list them with the script in the spec's Why section: entries of `commands.json` whose `params_schema.additionalProperties` is an object), find the axum handler mounted at the route path, read its `Query<T>` (or raw query parsing), and declare one `QueryParam` per accepted field with type, bounds and enum values as the handler enforces them. Put a `// handler: <file>::<fn>` comment on each declaration. A handler that takes no query gets an empty list. Shared lists (pagination) are `const` slices.
- [ ] **Step 5:** Fix the existing test `forward_builds_each_body_kind` only if it passes a param the handler does not accept; otherwise it must pass unchanged.
- [ ] **Step 6:** Run `timeout 900 cargo nextest run -p aq-command-registry`. Expected: all pass.

### Task 3: aqv2 stubs and hidden groups

**Files:**
- Modify: `crates/aqv2/src/cli.rs`, `crates/aqv2/src/client.rs`
- Test: `crates/aqv2/tests/cli.rs`

**Interfaces:**
- Consumes: the command surface client in `crates/aqv2/src/surface.rs` (request builder with `.subject(kind, id, revision)`).
- Produces CLI: `tasks cancel ID --project P [--revision R]`, `tasks retry ID --project P [--revision R]`, `workers revoke ID [--revision R]`.

- [ ] **Step 1: Failing tests** in `cli.rs`: change `unsupported_mutations_never_call_legacy_routes` to cover only `workers enroll` and `projects onboard`; add `stubbed_mutations_run_registered_commands`, which asserts against the test server that each of the three sends `POST /api/v1/commands/<name>` with the subject type, id and the given `--revision` as revision and expected version, and that without `--revision` one read precedes the post and its revision is used. Add `plural_groups_are_hidden`: `aq --help` contains neither `tasks`, `messages` nor `operations` as a listed subcommand, and `aq tasks list` still succeeds.
- [ ] **Step 2:** Run `timeout 900 cargo nextest run -p aqv2`. Expected: new tests fail.
- [ ] **Step 3:** Implement. Subject types and payloads come from each action's schema (`protocol/dashboard-actions/v1/schema.json` is only the envelope; the payload schemas are in `aq_server::commands::document()` inputs, visible with `aq run NAME --describe`). The revision read uses the registered read for the subject where one exists (`task.show`), otherwise the route the group's `list`/`show` already uses. Remove the three from `Command::unsupported`. Add `#[command(hide = true)]` to `Tasks`, `Messages`, `Operations`.
- [ ] **Step 4:** Run `timeout 900 cargo nextest run -p aqv2`. Expected: all pass, golden files updated only where help text changed.

### Task 4: Regenerate, document, verify

**Files:**
- Modify: `protocol/commands/v1/commands.json` (generated), `protocol/commands/v1/README.md` if it describes query pass-through, `crates/aqv2/README.md` for the three commands.

- [ ] **Step 1:** Regenerate (command in Global Constraints). Expected: `commands::` tests pass.
- [ ] **Step 2:** `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^agent_commands::/)'` and `--test it_server`. Expected: pass with no test edits; a failure caused by an undeclared param means a list from Task 2 is incomplete, so fix the list.
- [ ] **Step 3:** `python3 scripts/build-budget.py`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`. Expected: clean.
- [ ] **Step 4:** Commit when the owner asks.
