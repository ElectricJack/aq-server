# G1b — existing dashboard actions

Task: `steady-cascade-45.2`. Implementation stays on the assigned task branch.
The server publishes `task.create`, `decision.resolve` and
`supervisor.message.send` through `POST /api/v1/actions` and original-ID lookup
through `GET /api/v1/operations/{id}`. Dashboard caller migration belongs to G3/G8.

The adapters call the existing fixed-key human Graph operation owner, protected
exact review owner and durable global mailbox owner. The dashboard journal stores
admission identity, disposition and bounded references; it does not retain another
copy of task/message/decision intent. Legacy HTTP handlers share that admission
path and keep native response envelopes. Native preflight rejects changed intent
before consuming an ID already held in a domain journal. Native task byte-sensitive
replay remains in force; union task payloads use sorted JSON native bytes.

Task IDs equal operation IDs. The existing journal persists the original mapped
Graph request before dispatch, validates actual Graph receipts, and limits recovery
to the original fixed key. Read-only union lookup never dispatches. Explicit native
recovery can complete an ambiguous operation, which subsequent lookup reconciles.
Decision requests bind the exact decision revision and subject artifact; the native
review transaction checks the subject under authority locks and fences the full
current authenticated actor. Supervisor confirmation means durable enqueue only;
message ID remains caller-controlled and distinct from operation ID. Native
receipts retain `host_delivery: not_attempted` and no agent receipt.

Owner bootstrap enables `dashboard.actions` and keeps specific mutation capability
checks. Its envelope remains `dashboard-read-1`; a separate contract-reference enum
admits both read and action versions. The bounded principal-filtered SQL query
returns up to 100 unresolved IDs ordered by creation time and UUID, excludes
terminal/retired/foreign rows, and reports partial coverage on truncation.
Discovery combines native human Graph history with union history, deduplicating
IDs against the union journal. An authorized native-only lookup imports admission
metadata without dispatching or retaining another copy of native intent. Current
role and authorization are checked again before delivery. Lookup rechecks current
scope and returned references and gives neutral absence for revoked access.

The generated server schema includes exactly three closed payloads. Fixtures cover
one request per kind, all five dispositions, errors 401/403/404/409/410/413/422/426/
429/503, expired cursor restart, equivalent replay and changed-payload conflict,
invalid requests, and read/action envelope compatibility. The validator checks UTF-8
byte bounds in addition to JSON Schema character bounds and verifies schema digests.
G1b changes no dashboard asset, architecture/spec document, unrelated action
family, migration or dependency pin. The authorized integration merge also brings
in the current upstream B3/E0 changes; those retain their existing ancestry.

## Validation

All 15 final focused checks and the exact workspace acceptance command passed
against correction commit `032e481`, after the authorized
integration merge `47f6181`. No source changes followed that run. The final evidence
commit contains this document and the completed plan; AQ's close record captures
the delivered HEAD.

Environment: Linux; rustc 1.96.1 (`31fca3adb`), Cargo 1.96.1 (`356927216`),
Python 3.12.3 and pytest 9.1.1. Cargo build, test, lint and schema commands use `--locked`,
`CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`. SQL tests use disposable databases
on the local PostgreSQL fixture at `127.0.0.1:55432`. The one ignored real Graph test
was explicitly selected and passed with a copied local Graph fixture binary,
`AQ_TEST_GRAPH_BINARY=.aq/tmp/g1b/pinned/aq-graph` (absolute path in execution) and
`AQ_TEST_GRAPH_SHA256=882f7003a264ad322134113a35df319e346034191d7cfaa0960c85cb219c5167`.
It uses real Graph mutations and receipts behind a forwarding boundary that can
hold or lose a real reply. Protected review tests use actual SQL owners with
synthetic launch/host fixtures; they do not claim a provider run.

AQ admission: `aq test .aq/tmp/g1b/test_checks.py`, executed by the inline plan's
`task-done ... 1 e6babba -- aq test ...` hook. The adapter runs the commands below
serially with the stated environment, captures output, and asserts every subprocess
exit code is zero. Final scratch evidence: `.aq/tmp/g1b/complete-green.log`.
The installed AQ runner supports pytest rather than a Cargo
backend, so the adapter supplies resource admission while preserving exact Cargo
commands. CI owns unrelated full-suite coverage.

| Command | Result |
| --- | --- |
| `cargo clippy --locked --all-targets -- -D warnings` | passed |
| `cargo test --locked -p aq-server --lib dashboard_api::actions` | ok. 14 passed; 0 failed; 0 ignored; 0 measured; 168 filtered out; finished in 9.63s |
| `cargo test --locked -p aq-server --test human_task_create actual_graph_creation_response_loss_recovery_and_authority_fences -- --ignored` | ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 5.63s |
| `cargo test --locked -p aq-server --test dashboard_actions --test supervisor_conversation --test human_task_create` | ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.21s; ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.17s; ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.11s |
| `cargo test --locked -p aq-server --test profiled_results review` | ok. 4 passed; 0 failed; 0 ignored; 0 measured; 56 filtered out; finished in 4.63s |
| `cargo test --locked -p aq-server --lib readiness::tests` | ok. 2 passed; 0 failed; 0 ignored; 0 measured; 180 filtered out; finished in 1.01s |
| `cargo test --locked -p aq-server --test profiled_results protected_engine_question` | ok. 1 passed; 0 failed; 0 ignored; 0 measured; 59 filtered out; finished in 6.61s |
| `cargo test --locked -p aq-server --test profiled_results bounded_owner_quantum` | ok. 1 passed; 0 failed; 0 ignored; 0 measured; 59 filtered out; finished in 2.81s |
| `cargo fmt --check` | passed |
| `cargo test --locked --workspace --test dashboard_actions bootstrap_lists_pending_operations` | ok. 1 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 1.14s |
| `cargo test --locked -p aq-server --test dashboard_read --test dashboard_mounts --test readiness --test native_cancellation --test schema43_cancellation` | ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.30s; ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.40s; ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.70s; ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.14s; ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.28s |
| `cargo run --locked -p aq-server --example dashboard_actions_contract -- --check` | passed |
| `cargo run --locked -p aq-server --example dashboard_contract -- --check` | passed |
| `python protocol/dashboard-actions/v1/check.py` | passed |
| `python protocol/dashboard-read/v1/check.py` | passed |

The exact declared acceptance command,
`cargo test --locked --workspace bootstrap_lists_pending_operations`, also passed
through the same AQ-admitted adapter after publication of `9a73b5d` (unchanged
source). It ran `bootstrap_lists_pending_operations` successfully; other workspace
tests were filtered out. Compile and execution took 681.34 seconds. The log is
`.aq/tmp/g1b/exact-acceptance.log`. AQ required this exact spelling in the close
record in addition to the previously passing explicit-target command.

The regular human-task suite reports the pinned Graph test ignored; its separate
explicit selection in the table passed. Action fixtures cover three valid requests,
five dispositions, ten required errors, an expired cursor, identical/changed replay,
eleven invalid cases and read/action compatibility. The read validator covers five
synthetic actual-route fixtures. Both generated schema drift checks passed.

Published schema SHA-256 values:

- actions: `b477d348f4a1f8ff2302d773bfd01eca55f5cd1905da649eeef066561fdb7732`
- read: `a913e589689043d743ca8ccbe756f34fb7a2beb0f44d14a3cb089de777ca00fb`

Initial mounted tests failed the missing registry, disabled capability and empty
pending list. Native supervisor regressions exposed loss of conflict envelopes
and the escaped-body limit. The held-response fixture exposed a changed native
error status. The new terminal replay regression failed with a confirmed response
under stale authority, then passed after removing its early return. A missing test
helper import and two upstream readiness test read-count lints were corrected.
All final results above are from the corrected post-merge source.

`cargo fmt --all` cannot discover vendored crates' workspace membership here;
`cargo fmt --check` passes for workspace source. G1b leaves vendors unchanged.
No browser run, live provider execution, supervisor host delivery, agent receipt,
or unrelated full-suite result is claimed. Dashboard callers and downstream fixture
pinning remain G3/G8 scope.

## Source review and dispositions

One fresh-context reviewer (gpt-6-astra, high) reviewed the concrete source snapshot
`e6babba..891adf6` against the plan. Its verdict was ready with fixes. It deliberately
declined final generated-artifact validation and check results, which remained the
executor's responsibility. All four Important findings were accepted:

1. Explicit authority refusal could be replaced by the cached admission response.
   Cached fallback is now limited to server failures and reauthorizes current
   lookup access under a bounded timeout before disclosing any operation metadata.
   Held-response union and outer-deadline tests cover revocation and restoration.
   A further terminal-replay regression reproduced an early-return bypass (13
   other action unit tests passed). Terminal responses now reach the same final
   lookup fence. The post-merge correction is `032e481`.
2. A native task could commit, then fail an owner fence and become terminal union
   Conflict. Native retained evidence now determines classification; an unavailable
   post-admission fence retains Unknown. Restored ownership can reconcile the
   confirmed native task through GET without another Graph mutation.
3. Native supervisor revision/message conflicts lost their retained `not_queued`
   envelopes. The legacy path now returns the validated native receipt and checks
   its request digest. Existing native tests also exposed escaped JSON overhead;
   native supervisor requests retain their 128 KiB envelope, while union HTTP
   retains 64 KiB. UTF-8 body limits are unchanged.
4. Pre-upgrade unresolved Graph history was omitted with complete coverage.
   Bootstrap now combines both journals, and native-only lookup validates retained
   evidence and imports metadata without dispatch. Tests cover native-only
   unknown history, duplicate IDs, foreign principals, terminal exclusions and
   combined truncation.

No minor findings were deferred. The reviewer did not attest the later correction
or integration commits; the executor owns their regression and post-merge checks.

## Execution rulings

- Execute inline under the claimed task's instruction to finish this session,
  without a second approval menu. Cost if wrong: unwanted changes are reviewable
  on the assigned leaf branch.
- Use AQ-admitted pytest adapters for Cargo with two build jobs and two Rust test
  threads; CI owns the unrelated full suite. Cost: indirect execution and omitted
  unrelated runtime coverage.
- Use workspace `cargo fmt` rather than `cargo fmt --all`, which fails vendored
  workspace discovery. Cost: unchanged vendored formatting is not checked.
- Preserve native task raw-byte identity; union task payloads use sorted JSON.
  Cost: a native caller after a union create needs the same canonical native bytes
  or should look up the known original ID.
- Review a concrete source-only WIP snapshot while artifact generation continues,
  then squash unpublished WIP history. Cost: the reviewer does not attest the final
  generated bytes; executor drift and fixture checks provide that evidence.

## Integration source

The required `origin/dev` does not exist. Supervisor message
`msg-db1c96b0687d4faab64f16fce1499c6d` explicitly authorized `origin/main`, instructed
fetch/merge/validate/publish/close, and prohibited creating a replacement dev branch.
`aq git pull --branch main` fetched and merged
`42e027971128d761b5c3096e15de9fa348dfa6d7` as
`47f6181c4b276c9e94998130728e2bb2361b24bb`, without conflicts. The feature
commit is `7fa9974`; post-merge authority/test corrections are `032e481`. G1a source ancestry
`e6babba` is preserved. The integration substitution must appear in the close summary.
