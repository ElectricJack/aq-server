# G4 search slice

Task 3 adds `/api/v1/search` to the authenticated dashboard read router. It always returns typed task, session, result, playbook, and knowledge groups, with per-source coverage and a usable authenticated continuation whenever `has_more` is true. `q` is required, trimmed, bounded to 256 input bytes, and rejects control characters; `types` is an optional comma-separated group filter. Scope and page sizes reuse existing dashboard parsing (default 25, maximum 100).

Search uses the configured Graph owner's sanitized task/knowledge list adapter, current exact session inspection ACLs, native-plus-legacy retained public result metadata, and the actual configured playbook driver's loaded-definition inspection. Result pins retain their report digest. Playbook IDs retain their true definition names and source digest pins. The existing numeric `Revision` and reference-only digest semantics are unchanged.

Graph search additionally checks returned hits against public title/body/current-summary fields, so a broader upstream metadata-only match cannot expose a hit. Excerpts remove control characters and cap at 512 Unicode scalars; titles cap at 256. Raw configuration, manifests, environments, credentials, encrypted requests and arbitrary metadata are neither searched locally nor returned. Unknown or unrequested owners have null observations, unknown totals, partial coverage and a stable source reason.

Search shares the router's G1a codec, four-snapshots-per-actor / 64-global retention caps, 30-second TTL, and 256-member maximum with existing session/project reads. Filters bind query and requested groups alongside principal, current authorization partition, scope, resource and page size. Per-group continuations advance the shared group offset. Replay returns the same observed members and pins; authenticated tokens use fresh encryption nonces. Every delivery rechecks actor/ACL partition, project binding revision and full identity, exact session inspection, and the loaded-definition inspection owner where applicable. Binding removal, identity changes, expiry and router restart refuse continuation.

Collection is bounded: at most eight Graph projects (nine pins read to detect overflow), 32 Graph candidates per project/type, 32 exact session reads, 25 retained results, and the owner's at-most-four loaded definitions. At most 48 hits per group are retained so one group cannot exhaust the shared snapshot. Owner calls have a 650 ms budget and the route has a four-second budget. The Graph adapter shares the existing two-connection dashboard SQL pool. Bounded samples and restricted owner coverage are explicitly partial; exhausting a local snapshot never claims upstream completeness.

## Validation

Environment: `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, `DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres`. Rust uses the committed lockfile. Commands ran through `aq test` admission with its unchanged six-worker default; each admitted Python wrapper runs Cargo commands sequentially. The Graph HTTP boundary is a strict protocol fixture, while sessions/results/playbooks use disposable SQL and actual owning services. These tests do not claim a live external Graph deployment or provider execution.

RED command before route implementation:

```
G4_CARGO_COMMAND='cargo test --locked -p aq-server --test dashboard_read search_validates_queries_and_returns_all_groups -- --exact' G4_CARGO_LOG=docs/implementation/evidence/G4/search-red.log aq test .aq/tmp/g4/test_cargo.py -q
```

The named test failed with 404 versus expected 200, 0 passed / 1 failed; complete output is retained in `evidence/G4/search-red.log`. The same focused command later passed 1/1 (`search-green-first.log`). Intermediate fixture/assertion failures are retained separately, including credential length, randomized cursor-byte comparison, and an attempted removal of a retained-attempt project. Fixtures were corrected to exercise valid owner boundaries.

Final verification commands, counts and commit are recorded in the Task 3 implementer report and completion update below. Task 4 owns combined focused regressions, all-target clippy and broader cross-endpoint qualification; full suites remain CI-owned by the controller's explicit scope. No canonical architecture/specification documents or writable lifecycle authority were changed.

## Completion evidence

Implementation commit: `8f02812191a4e9f827096689e9a46f658d15a258` (`feat(dashboard): add bounded owner-backed grouped search`).

`aq test .aq/tmp/g4/search-checks.py -q` ran owning binaries sequentially. After correcting the final Graph fixture, its affected binary alone was rerun via `AQ_DASHBOARD_WRITE_FIXTURES=1 G4_CARGO_COMMAND='cargo test --locked -p aq-server --test dashboard_search' G4_CARGO_LOG=docs/implementation/evidence/G4/search-http.log aq test .aq/tmp/g4/test_cargo.py -q`.

- `cargo test --locked -p aq-server --test dashboard_search`: 3 passed (`search-http.log`).
- `cargo test --locked -p aq-server --test dashboard_read`: 7 passed (`search-read.log`).
- `cargo test --locked -p aq-server --test profiled_results dashboard_search_uses_retained_result_metadata_and_digest`: 1 passed (`search-result.log`).
- `cargo test --locked -p aq-server --test playbook_activation dashboard_search_loaded_definition_uses_true_name_and_source_digest`: 1 passed (`search-playbook.log`).
- `cargo test --locked -p aq-server --test dashboard_mounts`: 3 passed (`search-mounts.log`).

`aq test .aq/tmp/g4/search-final.py -q` then passed with these sequential commands:

- `cargo test --locked -p aq-server --lib dashboard_api::runtime::tests`: 2 passed (`search-runtime.log`).
- `cargo run --locked -p aq-server --example dashboard_contract`: schema generated (`search-schema-generate.log`).
- `/home/jkern/dev/agent-queue2/.venv/bin/python protocol/dashboard-read/v1/check.py`: 12 valid fixture checks and 19 invalid mutations passed (`search-schema-check.log`).
- `cargo run --locked -p aq-server --example dashboard_contract -- --check`: no schema drift (`search-schema-drift.log`).
- `cargo fmt --check`: passed, no output (`search-fmt.log`).
- `cargo clippy --locked -p aq-server --lib --test dashboard_search --test dashboard_read --test dashboard_mounts --test profiled_results --test playbook_activation -- -D warnings`: passed without warnings (`search-clippy.log`).

All logs are under `docs/implementation/evidence/G4/`. Final distinct Rust count: **17 passed, 0 failed**. Bootstrap's read-contract digest was checked against the generated schema. `git diff --check` passed for source/contract/prose changes; raw Cargo logs retain their original trailing blank lines. No known blocking concern remains for this slice.
