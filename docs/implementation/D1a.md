# D1a: central candidate creation and publication

Status: revision 1 received adversarial request-changes; revision 2 addresses the publication fencing defect and source documentation correction. Revision 2 validation and publication evidence are recorded below. No review approval or default-branch delivery is claimed for this revision.

Task: `crisp-meadow-36.1`. Plan: [implementation plan](../superpowers/plans/2026-11-02-D1a-central-delivery.md).

The central service resolves ordered receipt IDs from server-owned source retention records. It never accepts worker-supplied candidate source OIDs, retention handles, remote URLs, credentials or arbitrary operation JSON. The configured central aq-git binding owns the target allowlist. D1b owns policy and reconciliation; D7 owns command adapters.

Publication records a destination hold and a durable dispatch marker before socket I/O. No receipt after that marker means unknown outcome, including a crash before the call. This conservative case needs D1b reconciliation; the service never automatically re-pushes. Exact replies are immutable byte receipts including their newline and all protocol fields. New read operation IDs obtain fresh observations; replay returns the old observation.

An error receipt is not proof that publication had no remote effect. After dispatch, only a validated success or an error with `operation_state: failed` and no uncertainty indication releases the destination hold. Missing, pending or contradictory complete states on error replies remain fenced. In particular, an unflagged `io_error` with `retry: safe_same_operation` and no operation state stays unknown: its exact receipt is retained and replayed without another socket call. This primitive intentionally has no code-only refusal allowlist; reconciliation remains D1b's responsibility.

Migration `0045_central_delivery.sql` adds immutable intents, responses and publish dispatch markers, plus destination holds keyed by repository and full branch across binding generations. The migration is registered in the store, state format 45 and Docker metadata. The public API is `DeliveryService::{create_candidate,publish,candidate_show,operation_show}`. Typed request construction admits only these operations; transport bounds are 1 MiB/8 MiB with a 35-second timeout. Successful mutation receipts require a matching operation identity. Candidate conflicts retain bounded hex path evidence. `already_present` explicitly requires `caused_by_this_operation:false`.

Complete but malformed replies are retained as evidence and return a protocol error. A malformed publication reply retains its destination hold. Partial or oversized replies cannot become trusted receipts; after a publish dispatch they remain unknown and fenced. Non-publication transport retries use the original frozen request even when a source-owner row has subsequently changed.

History: the unpublished plan and implementation were squashed into `2497218a`, based on `8079f7c1`. Integration source `origin/main` at `2917c84e` merged cleanly as `ac92adf9`, with no Rust changes. A second merge at `b4c82ce7` incorporated `origin/main` `ff56fbba`, including the independent credential fixture repair. Supervisor message `msg-c81e8d8d99de4a4b9d30f8a27565e549` explicitly waives the generic absent `origin/dev` instruction in favor of `origin/main`; the merged `.aq/PRIME.md` records the same project rule. Final published SHA is recorded in the review submission and task comment to avoid a self-referential commit hash.

Environment: Linux x86_64, repository-pinned Rust 1.96.1, disposable PostgreSQL 17 at loopback port 55432 (`scripts/test-db`). Cargo uses `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2` under the foreground AQ test semaphore via a one-test pytest subprocess wrapper. No full-suite run is claimed; worker validation scope requires focused/related checks.

Real Git qualification uses retained `vendor/aq-git-v0.2.0-linux-x86_64`, SHA-256 `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2`, and Git 2.56.0 extracted from the preloaded pinned `aq-git-a3b:0.2.0` runtime to `/tmp/d1a-runtime/aq-git-runtime`. The temporary extraction container is removed. The acceptance fixture creates only temporary repositories, source bundles and sockets.

Validation (2026-11-02, America/Los_Angeles):

| Command | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked -p aq-server --test source_retention delivery_` (initial red) | Expected E0432: missing delivery service | [TDD red](D1a-checks/red.log) |
| `cargo test --locked -p aq-server --test source_retention delivery_rejects_unretained_sources` (corrupt-row regression red) | Expected failure: NULL central receipt panicked; implementation now refuses missing receipt | [Regression red](D1a-checks/null-receipt-red.log) |
| `cargo test --locked --workspace candidate_from_retained_source_publishes_once` (before fixture repair) | Failed before test execution: existing `dashboard_aggregation.rs:35,53` initializers omit credential `allowed_backends` / `kind` | [Workspace acceptance](D1a-checks/workspace-acceptance.log) |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` (before fixture repair) | Unrelated E0063 fixture failure | [Initial workspace Clippy](D1a-checks/workspace-clippy.log) |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` (after merge) | Passed, warnings denied | [Final workspace Clippy](D1a-checks/workspace-clippy-final.log) |
| `cargo test --locked --workspace candidate_from_retained_source_publishes_once` (after merge) | Passed: named acceptance 1 passed; real qualification 1 explicitly ignored in this command | [Final acceptance](D1a-checks/workspace-acceptance-final.log) |
| `cargo clippy --locked -p aq-server --lib --test source_retention -- -D warnings` | Passed, warnings denied | [Focused Clippy](D1a-checks/focused-clippy.log) |
| `cargo fmt --check` and `git diff --check` | Passed | Executed after integration merges |

The workspace build failure was filed as **crisp-meadow-36.14**, outside D1a, under the parent epic with direct integration. The supervisor was notified. The fixture repair is now present in fetched `origin/main` at `ff56fbba` and was merged; workspace Clippy and the exact acceptance command both pass after that merge. The repaired fixture adds credential defaults without changing its assertions.

Final focused validation:

| Command | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked -p aq-server --lib delivery_` | 3 passed: both new protocol tests and one existing scope test matched by the filter | [Protocol](D1a-checks/protocol.log) |
| `cargo test --locked -p aq-server --lib state_format_is_the_latest_migration` | 1 passed | [Migration](D1a-checks/migration.log) |
| `cargo test --locked -p aq-server --test source_retention` | 16 passed, 0 failed, 2 explicitly ignored; named acceptance and NULL receipt regression passed | [Area](D1a-checks/source-retention-final.log) |
| `cargo test --locked -p aq-server --test source_retention candidate_from_retained_source_publishes_once_real_git -- --ignored --nocapture` | 1 passed, 0 ignored; fixture shut down cleanly | [Real aq-git](D1a-checks/real-final.log) |

The actual Git qualification uses the retained aq-git binary and Git 2.56.0 runtime. It deletes the worker checkout after importing retained source, creates and observes a candidate, rejects a stale publication, applies a conditional publication once, verifies the remote OID, checks concurrent/restarted replay and changed-input refusal, checks observed-presence noncausation, rejects an unlisted target, looks up the operation, and replays the stored candidate response after shutting down aq-git. No default-ignored test is counted as passing; the qualification runs explicitly.

The source-retention suite also verifies committed intent before I/O, immutable receipts, frozen request replay after source-row changes, malformed/bounded conflict results, and destination holds across indeterminate replies, dispatched errors and lost responses. The unit tests exercise exact 8 MiB response boundaries, missing newline, oversized requests and contradictory/uncorrelated envelopes.

`cargo fmt --all` cannot discover excluded vendored package workspaces in this nested worktree. The required ordinary workspace `cargo fmt` / `cargo fmt --check` succeeds. Full unrelated test execution is outside worker validation scope; the mandatory workspace name filter was executed exactly and passed after the independent fixture repair; its earlier compile failure is retained as history.

Resolved implementation findings (self-review, not the required adversarial verdict):

- **Medium, fixed:** a retained source row with SQL NULL `central_receipt` previously panicked in `DeliveryService::source`. `delivery_rejects_unretained_sources` deterministically mutates that column; the recorded red panic is now an `InvalidInput` refusal and the area suite passes. Both nullable receipt loads use checked extraction.
- **Low, fixed:** abruptly killing the real fixture guard left its engine holding the test capture pipe. The qualification now sends SIGTERM, awaits successful guard exit with a timeout, and has process-group cleanup on failure; the final qualification exits cleanly.
- **High build blocker, repaired independently:** current credential fields were missing from the existing dashboard aggregation fixture. The exact two failing workspace logs are preserved, and the repaired fixture was merged from main without changing its assertions.

Revision-1 review: the adversarial reviewer reported one blocking fencing defect and five nits. The owner-delegated decision requested the blocking fix and the source doc correction only; see the revision-2 evidence below. Policy, onboarding, reconciliation and playbook adapters remain outside this task.

Reproduce the AQ-controlled Cargo invocations with [the pytest subprocess wrapper](D1a-checks/run_cargo.py): set `DATABASE_URL`, `D1A_CHECK` to the exact Cargo command, and `D1A_CARGO_LOG` to a fresh log path, then run `aq test docs/implementation/D1a-checks/run_cargo.py -q`. The wrapper fixes Cargo jobs and Rust test threads at two, preserving AQ's test semaphore and marker/worker defaults. Real qualification also sets `AQ_TEST_GIT_CANDIDATE` and `AQ_TEST_GIT_EXECUTABLE` as described above.

## Revision 2 — fail closed on publication errors

Review `rev-steady-dune` revision 1 received `request_changes` in decision `ask-decision-7aa9a2a9ae3e4c70a54460891a177965`. This revision fixes **High/blocking** `cmt-8cb2ef76b0c4` in `DeliveryService::execute` and **Low/nit** `cmt-9a6a59171afb` in the `create_candidate` doc comment. The other four nits remain deferred to follow-up under the explicit reopen instructions; no additional behavior is changed for them.

Regression `delivery_publish_errors_require_conclusive_refusal_to_release_destination` uses the existing local socket peer and disposable database. The exact reviewer case sends `io_error`, null details, `safe_same_operation`, and no `operation_state` after verifying a committed dispatch marker. It checks exact receipt preservation, replay with a reconstructed service and no socket peer, retained destination hold, and `DestinationBlocked` for a fresh operation ID. Controls cover an unknown error code, pending/complete error states, a failed state with a dispatched flag, and a confirmed failed refusal allowing a subsequent publication.

Validation environment: Rust 1.96.1, Linux x86_64, disposable PostgreSQL 17 at loopback port 55432. The existing foreground AQ wrapper sets `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`. Fresh `origin/main` at `71e08197` was incorporated before final validation; `git merge origin/main` reported already up to date. Only the requested test filter was run. Earlier workspace acceptance and real-Git qualification results above remain historical revision-1 evidence, not rerun claims.

| Command | Revision-2 result | Evidence |
| --- | --- | --- |
| `cargo test --locked -p aq-server --test source_retention delivery_` before fix | 5 passed, new regression failed: hold count 0 vs required 1 for `io_error` with no state | [Red](D1a-checks/revision-2-red.log) |
| `cargo test --locked -p aq-server --test source_retention delivery_` after fix | 6 passed, 0 failed, 0 ignored; all six regression scenarios executed | [Green](D1a-checks/revision-2-green.log) |
| `cargo fmt --check` and `git diff --check` | Passed | Formatting verified after the correction |

The exact pushed revision SHA is recorded in the review submission and task comment to avoid a self-referential commit hash. Revision 2 submission resolves the two requested comments; it is not a new review approval or permission to merge.

## Follow-up: clear-meadow-17

Applied publication replies must match the ready OID in the immutable candidate creation receipt, including on replay. This validation context stays local; publication wire parameters are unchanged. Operation observations must match `payload.expected_binding_generation`; the top-level `generation` is aq-git's independent journal revision.

For `candidate.create`, `overloaded`, `io_error` and `state_busy` with `retry:safe_same_operation` and no uncertainty flags return an attempt error without creating a terminal delivery receipt. The caller can retry the same operation UUID and frozen bytes, even after source-owner state changes; changed inputs still conflict. Other responses remain immutable. Publication keeps its committed dispatch marker, immutable replies and no-redispatch rule; D1b owns publication reconciliation and retry policy (coordination message `msg-c56dcd0b65d1447b938f9e6652e6525b`).

Non-publication calls commit before socket I/O and hold no database connection or row lock during the exchange. Concurrent calls rely on aq-git's same-operation idempotency and return the first persisted terminal response. Destination holds use NFC, Unicode lowercase, then NFC, preserving the exact wire target. A short repository advisory lock also protects comparisons against legacy hold spellings. Empty and duplicate source receipt lists are rejected before creating an intent; distinct sources keep their order.

The four focused regressions in `tests/source_retention/delivery.rs` are `delivery_cross_checks_candidate_oid_and_binding_generation`, `delivery_candidate_transients_replay_same_frozen_operation`, `delivery_socket_exchange_releases_rows_and_pool_connection`, and `delivery_canonical_hold_targets_and_nonempty_unique_sources`. The socket test uses one pooled connection and gates overlapping calls on actual socket requests, without sleeps. Run them with the required `cargo test --locked -p aq-server --test source_retention delivery_` filter through the AQ wrapper above.

Validation: the required locked Cargo filter passed all 9 matching tests (0 failed, 0 ignored), including all four regressions, under `aq test docs/implementation/D1a-checks/run_cargo.py -q` with Cargo jobs and Rust test threads fixed at two. `cargo fmt --check` and `git diff --check` passed. [Targeted Cargo output](D1a-checks/followup.log).
