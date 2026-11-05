# G1a: action admission, recovery and shared human contracts

Task: `steady-cascade-45.1`. Source base: `86ca27966346a84dba674eaec129956d84ec85c5`.
The final publication SHA is recorded on the AQ task; this file is part of that
commit, so it cannot contain its own SHA.

## Delivered boundary

`POST /api/v1/actions` validates a bounded, duplicate-key-free request and a
finite, versioned typed registry. Production registers no kinds and the
`dashboard.actions` capability remains disabled. Test-only adapters exercise
real SQL admission and retained responses. Authentication, CSRF and same-origin
checks remain on the human router. Current adapter authorization is rechecked
before every admission/retry.

Migration 42 binds a non-nil operation UUID to its principal, scope and canonical
SHA-256 request digest before dispatch. A unique insert chooses one dispatcher.
Retransmissions never dispatch; a changed digest conflicts. Confirmed, rejected
and conflict receipts survive at least 90 days; pending and unknown survive
indefinitely. Immutable tombstones prevent retired IDs from executing again.
Database guards reject identity changes, deletion, truncation and early retirement;
startup checks require the journal and its guards.

`GET /api/v1/operations/{id}` returns the retained disposition only to its original
principal, after the registered kind's mandatory lookup policy rechecks current
scope and every returned reference. Missing policy or revoked access fails closed
with neutral 404; retired tombstones require separate scope-only authorization.
Missing and other-principal IDs return the same neutral 404. Responses
carry timestamps, scope, digest, subject/receipt/affected refs and typed recovery.
Admission failure cannot dispatch; interrupted requests stay pending, and effect
timeout/ambiguity stays unknown. Failed completion writes preserve the proven
admission, with a best-effort read of the committed disposition.

All `/api/v1/*` errors use a common bounded safe envelope, including framework,
CSRF and capacity refusals. Terminal server-time, Retry-After and Cache-Control
headers survive body normalization. Outer request cancellation after proven admission
returns the retained receipt through a request-local marker. Cursor restart now returns 410 with a typed restart
code. The two existing human cursor consumers share authenticated encryption,
binding principal, authorization partition, scope, normalized filters, sort,
page size, resource, snapshot, position and validity interval. Existing ACL and
snapshot checks remain at each consumer.

The generated action schema and README are in `protocol/dashboard-actions/v1/`.
The dashboard read schema adds optional operation retention metadata; existing
read fixtures remain valid. G1b owns action fixtures and bootstrap pending-operation
population. No production action migration or dashboard application work is claimed.

## Verification environment

Linux isolated AQ worktree; committed Cargo.lock; PostgreSQL from `scripts/test-db`
on localhost:55432. Each Cargo check runs as a subprocess of the single temporary
pytest adapter `.aq/tmp/g1a/test_cargo.py`, admitted by `aq test`, with
`CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`. The AQ runner has no native Cargo
backend. Commands and full local logs are under `.aq/tmp/g1a/`.

Initial negative evidence:

- `cargo test --locked -p aq-server --test dashboard_actions action_union_rejects_unknown_kind_with_envelope`: failed 404 versus expected 422 before routes existed (`red-route.log`).
- Production boundary tests: 3 failed for missing routes, retention metadata and legacy CSRF envelope (`red-boundary.log`).
- `cargo test --locked -p aq-server --lib dashboard_api::actions::tests`: scaffold failed four journal/recovery tests; contract/registry checks passed (`red-journal.log`).

All final checks below passed through the AQ adapter (`verification-final.log`,
exit 0). Required named tests each executed exactly one matching test.

The expanded recovery run passed 15/16 and reproduced outer deadline cancellation
returning 503 after proven admission (`red-outer-deadline.log`). A request-local
retained receipt now survives cancellation and is returned by outer middleware.
Clippy initially found a large new error value; boxing its private envelope
preserves the wire contract. A pre-existing checked-then-unwrapped option in
`profiled_execution/no_grant.rs` was replaced with equivalent pattern matching
to satisfy the required lint. Its existing phase absence tests are included.

## Verification limits and decisions

The worker's AQ validation policy requires focused tests and affected-area suites;
full suite execution belongs to CI. Named tests must actually execute, never merely
produce a zero-test success. Cargo target selectors narrow compilation while
preserving the workspace named-test acceptance checks.

`cargo fmt --check` passes (`fmt-final.log`). Five baseline import/module ordering
differences in `profiled_execution` and `codex_runtime_recipe` were normalized
without changing behavior to satisfy this required check. The optional stronger
`cargo fmt --all --check` hits a pre-existing nested vendored-workspace metadata
error (`fmt-initial.log`); it is not claimed as passing.

The task already authorizes implementation, guarded publication and the final
merge from the integration source. That explicit workflow takes precedence over
generic skill menus that ask whether to execute or publish. The supervisor later
identified `origin/main` as the source because `origin/dev` does not exist; this
worker only publishes its assigned leaf branch.


## Fresh source review and fix pass

A read-only gpt-6-astra reviewer inspected `86ca279..175c830`. G1a has no
cross-family AQ adversarial gate. The reviewer found no Critical or Minor issues
and three Important issues:

1. GET lookup lacked current scope/entity authorization after principal matching.
   Required `ActionKind::authorize_lookup` now gates both receipt and tombstone
   disclosure. The regression first observed 200 instead of 404 after revocation,
   and 410 instead of 404 when the kind policy was absent.
2. Error normalization discarded protocol clock/backoff headers. The regression
   first observed missing headers; safe headers now survive normalization.
3. Two new integration-test queries accessed private `Store.pool`. The compiler
   reproduced E0616; tests now use `f.db.pool().await`.

The first two regressions failed in `red-review.log` (15 passed, 3 failed); the
third failed compilation in `red-integration-compile.log` before the fix. The final green
run is the verification of this single review fix pass, with no second reviewer.

The reviewer set aside generated artifacts (verified by the executor), future
production effects/reconciliation (no production kinds), and G1b fixtures/consumer
migration (explicitly outside this task). The task explicitly requires common
errors throughout `/api/v1/*`; this takes precedence over the older reference
spec's terminal error-shape preservation. Terminal success/effect semantics remain
specialized; the mounted error test now expects 422 plus safe common fields and
retained terminal clock headers.

Clippy also exposed baseline lint failures. The equivalent `no_grant` pattern
match removes an unnecessary unwrap; intentional partial-use warnings are scoped
to a shared terminal-fixture helper and the phase absence test's fixture module.
Five baseline import/module ordering fixes are formatting-only. These limited
changes make the required repository-wide static checks reviewable and clean.

## Execution rulings

- Execute without another plan-approval menu: the task already authorizes complete
  implementation. Cost if mistaken: unwanted implementation, still confined to the leaf.
- Run Cargo through one AQ-admitted pytest adapter with two build jobs and two test
  threads: AQ has no Cargo backend. Cost: wrapper indirection; exact inner commands
  and their output are retained.
- Narrow named workspace checks with target selectors and run affected-area suites:
  AQ assigns the full suite to CI. Cost: unrelated test execution is deferred to CI.
- Normalize baseline format/lint issues described above to satisfy mandatory checks.
  Cost: small incidental diffs; the sole behavior-preserving production rewrite is
  covered by the existing phase absence suite.
- Start fresh source review while deterministic artifact generation/area verification
  is still running. Cost: reviewer does not attest the final artifacts; schema validity,
  drift and fixtures are checked separately before publication.
- Apply common errors to every v1 family as the explicit task requires. Cost: deliberate
  old terminal error-status/body compatibility change, with preserved protocol headers.
- Leave future production effects and G1b artifacts to their assigned work. Cost: this
  task alone provides no enabled production mutation capability.
- Use the task's guarded publication/merge/close workflow in place of generic finishing
  menus. Cost: authorized leaf publication only; AQ integration owns default-branch delivery.


## Final verification

| Command | Result | Local log |
| --- | --- | --- |
| `cargo fmt --check` | pass | `fmt-final.log` |
| `cargo clippy --locked --all-targets -- -D warnings` | pass | `clippy-complete.log` |
| `cargo test --locked -p aq-server --lib dashboard_api` | 18 passed | `green-final-unit.log` |
| `cargo test --locked -p aq-server --test dashboard_actions --test dashboard_read --test human_graph --test dashboard_mounts` | 14 passed, 1 explicitly ignored pinned-Graph test | `green-areas.log` |
| `cargo test --locked -p aq-server --test phase_never_granted` | 2 passed | `phase-absence.log` |
| `cargo test --locked -p aq-server --test terminal_store actual_http_separates_listeners_csrf_identity_binding_and_sensitive_errors` | 1 passed | `terminal-errors.log` |
| `cargo test --locked --workspace unknown_effect_operation_is_recoverable_by_lookup --lib` | named test passed, 1 matching test | `t1.log` |
| `cargo test --locked --workspace action_union_rejects_unknown_kind_with_envelope --test dashboard_actions` | named test passed, 1 matching test | `t2.log` |
| `cargo test --locked -p aq-server --lib cursor` | 5 passed | `cursor-final.log` |
| `cargo test --locked -p aq-server --lib state_format_is_the_latest_migration` | 1 passed | `state-version.log` |
| `cargo run --locked -p aq-server --example dashboard_actions_contract -- --check` | schema drift check passed | `schema-validation.log` |
| `cargo run --locked -p aq-server --example dashboard_contract -- --check` | schema drift check passed | `schema-validation.log` |
| `python3 protocol/dashboard-read/v1/check.py` | 5 retained fixtures validated | `schema-validation.log` |

The temporary schema check also validated the published action schema with
Draft 2020-12, accepted a retained unknown receipt and safe error, rejected all
production action requests, and rejected missing fields/invalid disposition/time.
No committed action fixtures are claimed. The separately pinned real-Graph test
was not activated because its required binary/digest environment was not supplied;
existing wrapper/shared-codec tests passed. No full unrelated test suite ran.

## Publication checkpoint

The required `origin/dev` merge could not initially be performed: the ref was
absent locally and a read-only remote query returned exit 2/no matching branch.
Supervisor message `msg-39cc091a8741433e916f4e13834191c2` requests the correct source.
Supervisor ruling `msg-a1a4d4eab0904a308375dc7204a40c0a` authorized fetched
`origin/main` in place of nonexistent `origin/dev`, explicitly prohibiting creation
of dev. The single feature commit `3244f2252b2f105ce86b994703b1aa0c86db96ca` was
already published. `aq git pull --branch main` fetched and merged
`cc7b74e6a723a6dade7b632af514e6d90b7c7abb` without conflicts, producing merge
`fae3e8ca3fb8c600c42711bf6cc9d39b20eb33a0`. Incoming changes are planning documents
and scripts; G1a runtime code is unchanged.

Post-merge focused validation through AQ repeated the dashboard unit suite
(18 passed), route area suite (14 passed, the same pinned-Graph test ignored),
and schema validity/drift/read-fixture checks (all passed). Commands are the
corresponding entries above; logs are `.aq/tmp/g1a/post-merge{,-unit,-area,-schema}.log`.
The incoming planning tests initially failed because sibling historical Graph
evidence was absent from the default checkout layout. Their documented
`AQ_PLAN_PLATFORM_ROOT=/home/jkern/dev/aq-platform` reference setting made
`aq test scripts/test-v2-plan.py -q` pass all four tests (`post-merge-plan.log`),
without changing tests or adding a runtime dependency.
The AQ task records the final publication SHA and close result. Leaf publication
does not claim delivery to the default branch.
