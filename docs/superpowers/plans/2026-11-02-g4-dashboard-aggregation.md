# Dashboard Aggregation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete G4: truthful overview counts/attention/outcomes, bounded grouped search, and provider-account quota with unknown values preserved.

**Architecture:** Extend the existing dashboard-read contract and router. Read domain owners through their authorized projections and bounded snapshots; keep each source's observation and failure distinct. Keep quota collection in a router-owned account cache, with server-only authentication and a qualified read-only upstream adapter.

**Tech Stack:** Rust 2024, axum, Tokio, SQLx/PostgreSQL, reqwest, serde, schemars, the existing dashboard cursor codec.

**Spec:** The complete description of held AQ task `steady-cascade-45.5`, recorded below; plan contract `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml`, key G4.

## Global Constraints

Every task's requirements include these.

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Contracts:** a wire change publishes schemas and fixtures from the contract owner, and both endpoints consume a pinned version. The task is not done until valid, invalid, replay and compatibility cases pass on both sides (ecosystem §3).
- **Authority:** never add a second writable task lifecycle. aq-graph owns task state; AQ owns grants, attempts and dispositions (ecosystem §2).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Hosting:** every real agent CLI runs in an attempt-owned tmux host under the guardian (H1), inside its own sandbox (SBX1). Central supervisors run in the supervisor host (DEP5).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Isolation claims:** do not claim hostile-code isolation for shared-kernel containers (W6, SBX10).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- **Definition of done:**
  - `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` pass for the touched Rust workspaces. For aq-dashboard: `pnpm lint`, `pnpm typecheck` and `pnpm test`.
  - Evidence goes in `docs/implementation/<TASK-KEY>.md` in the owning repository, recording exact commands, environment, commit and remaining limits.
  - Changes are pushed through the project's integration policy.
  - Prior qualification statements are not reused for changed code.
- **Naming:** while v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- **Review:** tasks marked `review: adversarial` require a cross-family adversarial review (`aq-reviews` recipe) before a passing close. They integrate by pull request; all other tasks integrate `direct`.
- **Named tests are the evidence:** each test deliverable names a specific test. It is met only when that test exists and its run reports it passed. A filter that matches zero tests is an unmet deliverable. The owner or supervisor reviews any `pass` close that declares `--deliverable-unmet`.
- **Docs steward:** only A-series and docs tasks edit `aq-server/docs/architecture/*` or any repository's `docs/specs/*`. Other tasks record design-document changes in their evidence and file a `docs` task, so parallel worktrees don't conflict on the canonical specs.

## Execution rules and file map

This is an already-isolated AQ worktree on `aq/steady-cascade-45.5`, source base `fcb387bef76afba0489c3b84a0ab8f7a170d760b`. Keep that branch and injected git attribution. Do not change canonical architecture/spec documents, siblings, lifecycle ownership, or out-of-scope UI/results-feed/accounting/notifications features. The task is not marked `review: adversarial`; ordinary implementation review still applies. Do not spawn nested agents or send AQ messages from a delegated implementation.

- Modify `crates/aq-server/src/dashboard_api/contract.rs`: public typed read DTOs and response schema union.
- Modify `crates/aq-server/src/dashboard_api.rs`: authenticated bounded routes and query parsing; mounted-owner metadata as needed.
- Modify `crates/aq-server/src/dashboard_api/service.rs`: reuse auth/fence/envelope; delegate new projections to focused modules rather than adding another large inline service.
- Create `crates/aq-server/src/dashboard_api/aggregation.rs`: authorized overview contributions and source observations.
- Create `crates/aq-server/src/dashboard_api/search.rs`: bounded query-bound cursors, sanitized owner hits and stable grouping.
- Create `crates/aq-server/src/dashboard_api/quota.rs`: configured source, bounded authenticated collector, per-account deduplication/cache, and public unknown/stale projection.
- Modify `crates/aq-server/src/dashboard_api/runtime.rs` and `cursor.rs` only as necessary to share G1a cursor guarantees with search.
- Modify `crates/aq-server/src/config.rs` and explicit Config initializers only as necessary for opt-in quota source configuration; defaults preserve current startup.
- Modify `crates/aq-server/src/agent_access/human.rs` only to expose a bounded sanitized internal read adapter, keeping owner/project fences.
- Reuse review inbox, common attempt authority, worker heartbeat inventory and retained result/settlement projections. Expose narrowly scoped internal helpers when owner APIs need adaptation. Never read encrypted payloads as plaintext or infer readiness from active task state.
- Create `crates/aq-server/tests/dashboard_aggregation.rs` for actual HTTP/auth/SQL/Graph integration probes; extend the existing real encrypted review/result fixture in `tests/profiled_results.rs` and `tests/profiled_results/review_inbox.rs` for the overview named test rather than copying it. Add focused unit tests beside pure projections and collector parsing.
- Regenerate `protocol/dashboard-read/v1/schema.json` from `contract::schema_bytes()`, publish fixtures under `protocol/dashboard-read/v1/fixtures/` and verify valid/invalid/replay/compatibility cases.
- Write final command/environment/source/support/commit/limits evidence to `docs/implementation/G4.md`, preserving complete validation logs in `docs/implementation/evidence/G4/` where useful.

## Shared interfaces

Implement the shared contract sequentially in four bounded slices. Each slice receives a fresh implementation context and task review; final review covers the whole contract. Do not run concurrent implementers against shared files.

**Consumes:** `AppState`, `dashboard_api::Runtime`, `store::User`, `contract::{Scope,Envelope,ProjectRef,EntityRef,TitledRef,Observation,Total}`, G1a `cursor::{Binding,CursorCodec}`, Graph `scheduling_ready_current`, review inbox, exact result/settlement reads, common attempt authority and worker heartbeat owner.

**Produces:** `/api/v1/overview` -> `Envelope<Overview>`, `/api/v1/search` -> `Envelope<Search>`, `/api/v1/provider-usage` -> `Envelope<Vec<ProviderUsage>>`, generated read schema/fixtures, truthful `search.read` and `provider.quota` bootstrap capabilities.

## Task 1: Provider quota and shared DTO foundation

- [ ] Add `quota_unknown_is_not_zero` with configured accounts lacking a qualified source. Exercise the HTTP route with different project filters and assert identical account data, null numeric values and unknown state. Test source errors, independently missing windows, stale cached values, deduplication and no authentication leakage.

```rust
assert_eq!(window["state"], "unknown");
for field in ["used_percent", "used", "limit"] {
    assert!(window[field].is_null(), "{field}: {window}");
}
assert_eq!(usage["scope"], "provider_account");
```

- [ ] Run `cargo test --locked --workspace --test dashboard_aggregation quota_unknown_is_not_zero` under AQ's wrapper and retain RED.
- [ ] Add public `ProviderUsage` and `QuotaWindow` exactly matching the held task. `used_percent` is in [0,100] or null, absolute usage/limit stay null when upstream reports only percent, and reset/start times can be null. Keep every reported window distinct; choose a deterministic preferred window on the server.
- [ ] Mount a router-owned collector. Configured inventory is deduplicated by `(provider_id,account_ref)`; no-source accounts get an unknown window. Add an explicit opt-in source configuration referencing an existing account and protected auth file. Fixed upstream authority, no arbitrary redirect/SSRF/auth forwarding, strict response/time/size bounds, short cache, account-level single-flight, and sanitized errors. No live credential inspection or real provider calls in tests.
- [ ] Qualify the real Codex account quota source used by the upstream OpenAI Codex backend client: authenticated read-only ChatGPT backend usage with primary/secondary (and additional when present) windows, using upstream `used_percent` directly. Confirm the exact endpoint and auth/header fields against the current primary upstream source before coding; archive only public contract snippets/links, not keys. Optional source config names account explicitly and owns OAuth token-file loading. Expired/missing credentials and malformed/network responses produce unknown or stale last-success observations rather than page failure. No token/cost/slot inference, no OAuth scope widening, and no refreshing or printing user credentials. Other providers remain explicitly unsupported/unknown until separately qualified.
- [ ] Test the actual HTTP collector against a loopback mock with synthetic auth, including auth header ownership, bounded faults, redirect refusal, missing windows, deduplication, stale timestamp retention, account mismatch refusal and number validation. Fixtures qualify protocol behavior only; evidence must distinguish this from an authenticated live-account drill.
- [ ] Turn `provider.quota` on exactly when the collector is mounted and authorized, and `search.read` on when its route is mounted with usable authorization. Re-run the named quota test and the related area tests.

## Task 2: Owner-backed overview aggregation

- [ ] Add `overview_attention_lists_waiting_decision_and_failed_task` using the existing PostgreSQL fixture helpers and real router. Seed an authorized pending review through its owner (not forged encrypted SQL), and a retained failed attempt/result. Assert both pinned attention subjects, correct project, nonzero observed counts, and outcome time. Add a foreign project/actor case that cannot appear in the authorized output.

```rust
let (status, body) = read(&app, "/api/v1/overview", "owner@localhost").await;
assert_eq!(status, StatusCode::OK, "{body}");
let items = body["data"]["attention"].as_array().unwrap();
assert!(items.iter().any(|v| v["kind"] == "decision"));
assert!(items.iter().any(|v| v["kind"] == "failed"));
assert!(items.iter().all(|v| v["subject"]["revision"].is_string()));
```

- [ ] Run `cargo test --locked --workspace --test profiled_results overview_attention_lists_waiting_decision_and_failed_task` under the AQ resource wrapper. Selecting the owning test binary avoids compiling unrelated integration binaries while preserving the named deliverable. Record a real failing assertion or missing-type failure, never zero-test success.
- [ ] Introduce typed `AttentionKind` (`decision`, `question`, `blocked`, `failed`, `uncertain_operation`, `worker_offline`), `AttentionItem`, `ActionDescriptor`, `RecentOutcome`, and per-source freshness metadata. `Total` supports `exact`, `estimated`, `unknown`; failed contributions keep null totals. An action descriptor is a typed server-advertised available action or null and must not imply authorization merely from attention presence.
- [ ] Build scoped source contributions from actual owner reads. Graph readiness comes from its ready owner, attempts supply running/recovering/failed observations, review decisions enforce requester/eligible-owner visibility, and uncertain operations are the current principal's. Worker offline observations are restricted to authorized project associations. Results/settlement outcomes label their actual meaning and never call an unaccepted process result accepted. Questions without a mounted owner remain an explicit unavailable contribution, not fabricated empty data.
- [ ] Bound project/member reads and local SQL timeouts. Count exact complete sources, mark bounded samples estimated, and make failed or semantically unavailable sources unknown. Aggregate partial coverage with static reasons and retain observations per source. Fence current authorization and project binding after awaits.
- [ ] Re-run the named test and overview area tests; cover no data, unavailable Graph/reviews/results, truncated reads, actor exclusions and per-source timestamps. Update the legacy placeholder assertion only when it reflects the new real owner behavior.

## Task 3: Bounded grouped search

- [ ] Add HTTP tests for grouped task/session/result/playbook/knowledge hits with sanitized excerpts. Use small explicit sample titles and canaries in secret-bearing metadata/configuration. Assert no canaries in the response, exact scope and current ACL enforcement, rejection of empty/control/oversize query, bounded page size, and query/scope/actor/limit cursor binding.

```rust
let (status, body) = read(&app, "/api/v1/search?q=example&limit=1", "owner@localhost").await;
assert_eq!(status, StatusCode::OK, "{body}");
let groups = body["data"]["groups"].as_array().unwrap();
assert!(groups.iter().all(|g| g["hits"].is_array() && g["has_more"].is_boolean()));
assert!(!body.to_string().contains("private-search-canary"));
```

- [ ] Run the focused search tests and record RED before adding the route.
- [ ] Introduce typed `SearchType`, `SearchHit {ref,title,excerpt,project}`, `SearchGroup {type,hits,has_more}` and `Search {groups}`. Add optional continuation metadata as necessary so `has_more` always has a usable cursor. Use G1a authenticated cursors and bounded snapshots; bind query text, group filters, actor authorization partition, scope and page size. Keep typed hits pinned to the observed revisions. Validate query byte bound 256 and page sizes through the existing runtime limits.
- [ ] Implement owner-backed bounded searches. Graph searches only visible configured workspace bindings and whitelists display fields; session hits use exact session visibility; result hits use retained public metadata, playbook hits use inspectable loaded-definition names/purposes, knowledge hits use Graph's knowledge search. Do not search raw configuration, credentials, manifests, environments, encrypted requests or arbitrary metadata. An unavailable owner contributes an empty group only with partial coverage and a source reason.
- [ ] Re-run search tests including next-page replay, changed-query rejection, expired/restarted snapshots, denied actor and partial-owner behavior.

## Task 4: Contract fixtures and integrated verification

- [x] Generate schema from the final DTOs with the existing schema generator/test pattern; add nonempty, empty, partial, stale and unknown fixtures. Add typed schema validation for each and invalid percent/enum/extra-field examples, replayed cursors, and prior read fixture compatibility.
- [ ] Controller: record downstream aq-dashboard pin/consumer follow-up in a `CROSS-REPO FOLLOW-UP (aq-dashboard):` task comment; do not edit another repository.
- [x] Self-review every goal/field and update this checklist. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and focused `cargo test --locked` checks (the named owning-binary tests and related dashboard/owner area tests) through `aq test` resource admission with `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2` and the disposable PostgreSQL at `postgres://postgres:test@127.0.0.1:55432/postgres`. Named commands must report at least one matching passed test. Preserve complete output in evidence logs; an unavailable check is not a pass. Project startup says full suites belong to CI: use the task's explicit checks and focused related suites, reporting any known baseline rather than weakening tests.
- [x] Write `docs/implementation/G4.md` with exact commands, environment, public source qualification links, provider support table, observed limitations, fixture paths, and the implementation commit reference. Commit source and evidence with injected attribution. Do not push, squash, merge dev, close AQ, or mutate task state from the delegated implementation; the controller owns final review/publication/lifecycle.

```bash
git add crates/aq-server/src crates/aq-server/tests/dashboard_aggregation.rs protocol/dashboard-read/v1 docs/implementation/G4.md docs/implementation/evidence/G4
git commit -m "feat(dashboard): aggregate overview, search and account quota"
```

- [ ] Controller: review implementation against the complete held task and this plan. Resolve findings, squash only own unpublished leaf commits while preserving the source-base ancestry, merge `origin/dev` into the task branch, run targeted checks for the merged files, publish with `aq git push`, record exact HEAD and close with evidence. Never bypass AQ denial or claim another task's work.
