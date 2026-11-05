# Task 3 implementer report

Status: DONE. All 17 focused Rust tests and final checks passed.
Base: `21342bb383d57d2cdf19287d0a3e125e8e120101`.
Branch: `aq/steady-cascade-45.5`.
Implementation commit: `8f02812191a4e9f827096689e9a46f658d15a258` — `feat(dashboard): add bounded owner-backed grouped search`.

## Changes

- Added authenticated `/api/v1/search` and typed `SearchType`, `SearchRef`, `SearchHit`, `SearchGroup`, `Search` response projections. Bootstrap now advertises the mounted search route when the bounded session authorization partition is complete.
- Query input rejects empty, control-containing and over-256-byte text, unknown types/fields and invalid page sizes. Trimming and sorted/deduplicated type filters determine the authenticated cursor binding. Existing scope and page limits are reused.
- Added bounded cached search data to the existing router runtime, using the same codec, 256-member bound, four snapshots per actor / 64 total, and 30-second TTL. Search query and group filters join existing actor, partition, resource, scope and limit bindings; old session/project cursor paths keep empty filters and their existing semantics.
- All five groups remain present. Per-group `has_more` matches a usable `next_cursor`; continuations advance the shared group offset across stable snapshot members. Tokens use randomized encryption, so replay equivalence compares members/pins/snapshot identity and verifies returned continuation tokens, rather than byte equality.
- Graph uses a narrow internal adapter around the existing schema-validated, sanitized human list reader and shares the dashboard's bounded SQL pool. Only visible configured project/workspace bindings are searched. Local public-field matching excludes metadata-only upstream matches; stale summaries are excluded unless their summary revision matches the observed content revision.
- Session hits reuse exact membership and per-session authorization. Every displayed cached session is re-authorized against its current identity. Result hits use `human_results::read_as` with the native-plus-legacy profile and only public conclusion/process outcome. Playbooks use the actual injected configured driver and `inspect_loaded_definitions`, preserving the true name and source digest rather than inventing UUIDs.
- Final delivery fences current actor/ACL partition and full project binding identity/revision; replay checks the loaded-definition namespace owner where relevant. Query/filter/scope/page changes require restart; changed actor/ACL/binding refuses delivery. Expired/restarted snapshots require restart.
- Samples are bounded to eight Graph projects, 32 Graph candidates per project/type, 32 sessions, 25 results, and the owner's at-most-four loaded definitions, with 48 retained hits per group. Source calls have a 650 ms budget and the route a four-second budget. Unknown/unrequested owners have null observations and explicit partial coverage; truncation and restricted source coverage never claim completeness.
- Published schema and actual search route fixture; updated only the bootstrap search capability and read-contract digest. Broader unrelated fixture regeneration was restored to the base contents for Task 4. No quota or overview semantics, canonical architecture/specification documents, or writable lifecycle authorities changed.

## TDD and failures retained

Before adding the route:

```
G4_CARGO_COMMAND='cargo test --locked -p aq-server --test dashboard_read search_validates_queries_and_returns_all_groups -- --exact' G4_CARGO_LOG=docs/implementation/evidence/G4/search-red.log aq test .aq/tmp/g4/test_cargo.py -q
```

RED: one actual test, 0 passed / 1 failed, HTTP 404 versus expected 200 at the absent route. The complete Cargo failure output is retained in `docs/implementation/evidence/G4/search-red.log`. The same command with `search-green-first.log` later passed 1/1 after implementation.

Additional complete failure logs were retained without overwriting RED:

- `search-http-fixture-failure.log`: the Graph stub token was shorter than the owner's credential minimum; lengthened the explicit canary credential.
- `search-http-assertion-failure.log`: randomized encrypted tokens prevented whole-response byte equality, and an ungranted synthetic fixture had no grant to unwrap. Replay now compares stable observed content and exercises both generated continuation tokens; sibling fixture handles optional grants.
- `search-http-removal-fixture-failure.log`: retained-attempt project deletion correctly hit a foreign key. Replaced that deletion fixture with a separately configured Graph project without retained attempt references.
- `search-http-project-fixture-failure.log`: the replacement fixture initially omitted non-null validation columns. It now copies the existing validated binding observation and timestamp.

These were fixture/assertion corrections; no production authorization bypass was introduced. Initial `cargo fmt --all` encountered the existing vendored-package workspace-membership issue; `cargo fmt -p aq-server` formatted the touched workspace, and the required final `cargo fmt --check` is reported below.

## Commands and results

All Cargo tests use `--locked`, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, and `DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres`. They were admitted via `aq test` with the unchanged six-worker default. Python wrappers execute commands sequentially, capture combined stdout/stderr, persist the complete output, and assert nonzero exits. No full suite or all-target run was added to this slice.

`aq test .aq/tmp/g4/search-checks.py -q` sequentially ran the following five owning targets. After the final Graph fixture correction, only its affected binary was rerun using:

```
G4_CARGO_COMMAND='cargo test --locked -p aq-server --test dashboard_search' G4_CARGO_LOG=docs/implementation/evidence/G4/search-http.log AQ_DASHBOARD_WRITE_FIXTURES=1 aq test .aq/tmp/g4/test_cargo.py -q
```

| Cargo command | Actual passing tests | Retained log |
| --- | ---: | --- |
| `cargo test --locked -p aq-server --test dashboard_search` | 3 | `search-http.log` |
| `cargo test --locked -p aq-server --test dashboard_read` | 7 | `search-read.log` |
| `cargo test --locked -p aq-server --test profiled_results dashboard_search_uses_retained_result_metadata_and_digest` | 1 | `search-result.log` |
| `cargo test --locked -p aq-server --test playbook_activation dashboard_search_loaded_definition_uses_true_name_and_source_digest` | 1 | `search-playbook.log` |
| `cargo test --locked -p aq-server --test dashboard_mounts` | 3 | `search-mounts.log` |

Specific Task 3 tests: `search_validates_queries_and_returns_all_groups`; `search_graph_groups_are_pinned_sanitized_scoped_and_replay_bound`; `search_session_visibility_uses_exact_current_acl_and_scope`; `search_session_cursor_refuses_revoked_acl_partition`; `human_results::dashboard_search_uses_retained_result_metadata_and_digest`; `definitions::dashboard_search_loaded_definition_uses_true_name_and_source_digest`. These named tests actually ran, with no zero-match filters.

The Graph case covers task and knowledge hits, exact content revision pins, public excerpts, metadata and credential canaries, page bounds, next-page and terminal-page continuation, replay, changed query/types/scope/actor/limit, unknown actor, expiry, router restart, binding revision and binding deletion, foreign scope and unavailable-owner partial coverage. The session cases cover absent/wrong-boot/exact/revoked ACLs, scope, and cursor partition revocation. The result and playbook cases exercise actual owning services and digest/name pins.

Final wrapper: `aq test .aq/tmp/g4/search-final.py -q`. It runs shared runtime unit tests, generates and checks the schema, validates retained compatible and invalid examples, runs `cargo fmt --check`, and scoped clippy. Final wrapper passed (one admitted Python test, 264.94 seconds). Its Rust unit-test filter ran two actual tests; every subsequent command exited zero, with no Rust warnings.

## Self-review and remaining limits

Reviewed the complete diff, current authorization fences, source-specific reference pins, bounded memory/SQL/request work, and absence of secret-bearing projection/search fields. Shared Graph SQL pooling, per-group fair sample caps, current-summary revision matching and null unknown observations were added during self-review. No outstanding known implementation defect.

Graph transport tests use a strict schema-valid protocol stub, not an independently launched live Graph binary. Session, result and playbook tests use disposable PostgreSQL and actual owner implementations; no provider execution is claimed. Sampling and startup/retained metadata coverage are explicit in the response. Task 4 runs combined focused regressions, all-target clippy, and wider both-endpoint fixture qualification; full suites remain CI-owned per the controller's explicit task boundary. Local commits only; no AQ lifecycle/messages, network publication, merges, resets or canonical docs edits.

## Files

Production: `crates/aq-server/src/dashboard_api/search.rs`, `dashboard_api.rs`, `dashboard_api/{contract,runtime,service}.rs`, `agent_access/human.rs`, and `lib.rs`.

Tests: `crates/aq-server/tests/dashboard_search.rs`, `dashboard_read.rs`, `dashboard_mounts.rs`, `profiled_results/human_results.rs`, and `playbook_activation/definitions.rs`.

Contract/evidence: `protocol/dashboard-read/v1/{schema.json,check.py,fixtures/search.json,fixtures/bootstrap.json}`, `docs/implementation/G4-search.md`, `docs/implementation/evidence/G4/search-*.log`, and this report.

## Final verification results

| Exact command | Result | Log under `docs/implementation/evidence/G4/` |
| --- | --- | --- |
| `cargo test --locked -p aq-server --lib dashboard_api::runtime::tests` | 2 passed, 0 failed, 182 filtered | `search-runtime.log` |
| `cargo run --locked -p aq-server --example dashboard_contract` | Generated owner schema, exit 0 | `search-schema-generate.log` |
| `/home/jkern/dev/agent-queue2/.venv/bin/python protocol/dashboard-read/v1/check.py` | 12 valid fixture checks and 19 invalid mutations passed | `search-schema-check.log` |
| `cargo run --locked -p aq-server --example dashboard_contract -- --check` | Schema bytes match generator, exit 0 | `search-schema-drift.log` |
| `cargo fmt --check` | Exit 0, no output | `search-fmt.log` |
| `cargo clippy --locked -p aq-server --lib --test dashboard_search --test dashboard_read --test dashboard_mounts --test profiled_results --test playbook_activation -- -D warnings` | Exit 0, no warnings | `search-clippy.log` |

Also verified `git diff --check` for source/contract changes and the bootstrap fixture's `dashboard-read` contract digest against SHA-256 of the generated schema bytes. Total distinct final Rust tests: **17 passed / 0 failed**. Named runtime tests: `dashboard_api::runtime::tests::snapshot_caps_are_per_principal_and_global_without_cross_actor_eviction` and `dashboard_api::runtime::tests::cursor_authenticates_actor_partition_scope_resource_limit_expiry_and_bytes`.

Remaining concerns: none blocking for this slice. The explicit source sampling/coverage limits and deferred integrated qualification are documented above. Evidence/report follow-up commit contains this report and all search logs; the implementation commit above contains every production, test and contract change.

Raw Cargo logs are retained byte-for-byte, including Cargo's trailing blank lines. Git whitespace checks report those EOF blank lines when raw logs are included; source, contract and prose diffs are clean.
