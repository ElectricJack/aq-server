# Retained publication recovery: source validation

This is source/test evidence for task `28a8069e-2559-47b5-b433-1eeaffb8e827`,
not a release, live reconciliation, result acceptance or hold override.
The sandbox has no Git metadata, so a Git HEAD cannot be reported. The exact
initial implementation snapshot fingerprint (2542 source/configuration files) is
`e910faf1916fd620f8b4d734c124a5fe8c95ed59b8f7b7337992a16aa7579a63`. It covers
all sorted paths from `rg --files --hidden` ending in `.rs`, `.sql`, `.toml`
or `.lock`, hashing each as `path UTF-8 + NUL + SHA256(file bytes)` and hashing
the concatenation with SHA256. Documentation is outside that code fingerprint.
The delivery-check follow-up at the end records the current repair snapshot
and distinguishes this inherited evidence from checks run in the follow-up.

The authenticated `result.review.show` read confirmed result
`68d19730-4db8-4668-855b-d3e9572d5050` at revision
`f4b26ac1956b58997c0faca587f675453b20c9fb127da61dbf701b038f2b7b19`,
repository `97a459e3-e35f-5af0-85c0-1c095b50519c` and retained head
`ac8b015d6dfaaa0aa7850d2e7ff13a2d20285cdd`. That is the original retained
work's identity, not the revision of this repair's tested code. The read still
reported empty `source_receipt_ids`; no publication acceptance is established.

## Changes

- Owner/project-supervisor commands inspect the exact report/repository and
  original private aq-git journal, with bounded fields and payload verification.
- Observation-only reconciliation freezes its original journal generation
  before dispatch, retains immutable recovery receipts and replays the same ID.
- Exact-result dispatch fencing prevents duplicate or substitute pushes after
  lost responses, crashes or repeated result-ready events. Source refs and
  leases are unchanged. Definitive refusals require an operator decision.
- Existing delivery resumes from a recovered receipt and keeps ordinary checks,
  source/effect evidence, review and settlement gates. Recovery cannot accept,
  force, release, replace a worker, resolve an operation or change permissions.
- Migration 0179 stores the immutable dispatch/request/receipt evidence. New
  tests use existing binaries; no build budget was edited.

## Checks

All final Cargo commands used `CARGO_TARGET_DIR=/tmp/aq-target` and
`CARGO_HOME=/tmp/aq-cargo`. No build artifacts were placed in the worktree.

| Exact command | Outcome |
| --- | --- |
| `cargo test -p aq-delivery --features delivery_playbook --lib github::tests::commands:: --locked` | 33 passed, including 7 retained-publication cases. |
| `cargo test -p aq-command-registry --features delivery_playbook,supervisor_full_access --lib --locked` | 23 passed. |
| `cargo test -p aq-supervisor-control --features delivery_playbook,supervisor_full_access --lib retained_publication --locked` | 1 passed: own-project paths accepted; cross-project paths refused. |
| `cargo clippy -p aq-server --features delivery_playbook,supervisor_full_access --test it_profiled --test it_playbook --locked -- -D warnings` | Passed; compiled both integration binaries' tests without warnings. |
| `cargo test -p aq-server --features delivery_playbook,supervisor_full_access --test it_server commands:: --locked` | 7 passed: registered routes, scope/subject schemas and command admission. |
| `cargo test -p aq-playbook-domain --features delivery_playbook --lib refusal_tests --locked` | 2 passed: bounded diagnostics and safe next steps. |
| `cargo clippy -p aq-delivery -p aq-command-registry -p aq-playbook-domain -p aq-supervisor-control -p aq-server --features aq-server/delivery_playbook,aq-server/supervisor_full_access --lib --locked -- -D warnings` | Passed. |
| `cargo clippy -p aq-delivery --features delivery_playbook,source_observation --lib --locked -- -D warnings` | Passed on the final dispatch-boundary implementation. |
| `cargo fmt --all -- --check` | Passed. |
| `python3 scripts/migration_catalog.py` | Passed; 179 contiguous migrations. |

The seven publisher cases cover immutable identity/repository binding, uncertain
success reconciliation with the original generation, a moved/ambiguous remote,
bounded original rejection evidence, cross-binding/payload-hash refusal,
capability-gated classification and recovery only from valid original success
OIDs/time. Socket fixtures assert that inspection/reconciliation sends no push.

Earlier iterations failed `cargo check -p aq-server --features
delivery_playbook,supervisor_full_access --tests --locked` on new test harness
type aliases/API signatures. Those were corrected; the final Clippy compilation
above passed. An earlier registry run failed because feature-gated command
routes were also added to the reviewed-route list. The overlap was removed,
and the source scan now respects the module's feature gate; the final registry
run above passed. The initial format check found formatting differences, which
were corrected before the passing final check. During the dispatch-boundary
refactor, a missing `Run` import temporarily prevented delivery/notice tests
from compiling; it was fixed before their final passing runs. Read-only
preflight no longer consumes the dispatch marker, and migration 0179 fences
historical requests before the upgraded driver runs.

## Unrun and operator-only prerequisites

`DATABASE_URL` is unset. The eight new database integration tests in
`profiled_results::auto_settlement56::playbook::publication_recovery` and the
held-delivery receipt regression in `playbook_host9` were compiled but not run.
They cover receipt replay/conflicts, refusal versus uncertain success, moved
refs, wrong project/repository/revision/generation, released claims, immutable
history/pagination, a lost receipt's frozen generation and duplicate dispatch
fencing and retryable read-only preflight before the push boundary. PostgreSQL
execution remains a required review/CI check.

CodeScene and `scripts/build-budget.py` were not run: the sandbox has no
CodeScene or `origin/main` history. Full workspace, deployment and live-runtime
qualification were not run. Migration numbering still needs the ordinary
pre-release check against current main; no migration history was fetched.

The scoped supervisor-host `operation.show` of original operation `dfd8cb53`
returned `not_found`; it is a different journal. The sandbox could not inspect
the original private publisher installation or confirm its runtime capability,
so the actual rejection class and any narrowly required remote-policy change
remain operator questions. No live reconciliation or publication was attempted.

The cross-project AQ message was refused (`not_found`). Two relay/progress
messages to global supervisor `ea5785f9-3da2-4bf4-8f64-e9558147b2f3` were queued
successfully. Later mailbox reads were unauthenticated; credentials were not
changed. The final completion/evidence handoff was subsequently queued
successfully as message `d9d0c4c1-0c03-4d87-ba89-5c07bcdfa510` (cursor 193).
Queued is not a claim that the recipient has read it.

Release requires the ordinary reviewed server/migration release and verification
of the original private socket/capability. Live recovery, any permission change
and any separately permitted retry require explicit operator decisions. The
[operator procedure](../operations/retained-publication-recovery.md) documents
the exact result and original operation identities. Existing holds and human
review gates remain in force.

## Delivery-check follow-up, 2026-11-25

The runner identifies the failed-check input as commit
`489acd898966ce1d9b07a61b753748ed55779e32`, result
`4424f991-19cd-4502-95e9-91b50cd20bf0`. There is no `.git` metadata here to
independently resolve that commit. Reconstructing the input from saved original
files yields the initial fingerprint above exactly. The current source snapshot
has 2543 files and fingerprint
`d739b0d8a306113336e16c7c083cb4244eea191a28e54fca86b89b9924a720b2`,
using the same algorithm. Neither fingerprint is the original retained worker
commit or evidence of a deployed runtime capability.

The supplied CI tail contains only PostgreSQL shutdown/container teardown,
without a failed job name, test or assertion. A request for the exact failure
was queued to the global supervisor (`eb8c123a-5742-4684-9fc7-8a54dc884ed8`).
Session relays cursors 1–7 were read and acknowledged, and acknowledgment plus
the exact nine test names was queued back as
`c4cdcf5b-c9ac-4ab8-b037-db08b4aef1a5` (global cursor 201). The relay reports
partial/incomplete native listings and `identity_unavailable` reads; it
did not supply the failed result's immutable revision or bound job/assertion.
A subsequent supported agent read succeeded (details below); the check
job/assertion remains unretrieved. This follow-up does not claim to have
reproduced the unspecified CI failure.

Source inspection found and corrected the bridge's use of `features` instead
of protocol-v1 `capability_tokens`. Tests now use the documented wire field
and refuse a token in the unrelated field. Stored summary strings and phase
labels are allowlisted; unknown text cannot escape through truncation. Valid
original success acknowledgments expose only bound OIDs/target/time. The four
original and later incident identities are inspected separately in socket
fixtures; no fixture sends a push or live operation.
The owner-relayed terminal task/result and both of its original/later journal
identities are separate evidence-only cases in the operator procedure. A
synthetic socket regression refuses crossing their task or source commit;
it establishes no live journal availability or common rejection cause.

A terminal read-only PR refusal is marked `publication_not_dispatched` by the
host. It does not fence a fresh first dispatch. Committed dispatch markers,
published outcomes and uncertain historical requests continue to fence every
substitute request. The existing closed/merged PR regression asserts that the
preflight created no dispatch, and a ninth recovery integration test checks
that the preflight exception cannot override a committed marker. Synthetic
forge fixtures now mirror their retained-result identity in storage, allowing
the production identity/FK checks to run normally during dispatch. The CI
feature-on selection now includes all nine recovery integration cases. The
existing supervisor registry/control and server-command CI steps also enable
`delivery_playbook,supervisor_full_access` to select project-scope refusal and
publication command routes with both feature gates active. CI execution is
unverified.

Follow-up commands use `CARGO_TARGET_DIR=/tmp/aq-target` and
`CARGO_HOME=/tmp/aq-cargo`. The initial delivery subset ran successfully
(33 passed) before follow-up edits; final snapshot checks are recorded below.
The final delivery library run passed all 116 tests, and the
compiled integration binary listed exactly the nine recovery tests below.

`DATABASE_URL` remains unset, and no PostgreSQL binaries are installed. A
temporary-package preflight failed because the sandbox's only mapped UID/GID
is root and it cannot switch to a test user (`setgroups`/`seteuid` refused;
`unshare --user --map-user=1000 --map-group=1000 id` also refused the UID map).
No host services, users or credentials were changed. Database execution stays
unrun and must pass CI. CodeScene, the main-history build-budget comparison,
full workspace checks and deployed private-socket capability remain unverified.
Release still requires normal review/checks, migration-number verification
against current main and supported access to the original installation's
private socket. Any live recovery or remote-policy correction requires a
separate operator decision; no such action was run or authorized here.

The nine unrun test names share prefix
`profiled_results::auto_settlement56::playbook::publication_recovery::`:

- `exact_result_refusal_and_uncertain_success_recover_an_idempotent_receipt_without_republication`
- `absent_or_moved_remote_retains_rejection_and_requires_a_new_observation_after_correction`
- `wrong_project_repository_result_revision_binding_and_cancelled_claim_are_refused`
- `definitive_rejection_is_evidence_not_permission_to_retry_or_release`
- `original_operation_history_is_bounded_and_can_be_read_without_dropping_identities`
- `a_lost_receipt_reuses_the_durably_frozen_publisher_generation`
- `committed_dispatch_marker_prevents_repeated_or_substitute_push_after_a_crash`
- `a_read_only_preflight_failure_does_not_consume_the_publication_dispatch`
- `a_proven_preflight_refusal_does_not_fence_a_fresh_request_but_a_dispatch_still_does`

Run against disposable PostgreSQL 17 (the existing CI service supplies it),
with test-administrator privileges for database templates/migrations and
`session_replication_role`, using:

```sh
cargo test -p aq-server --features delivery_playbook,project_environments --test it_profiled publication_recovery:: --locked
```

This command is **unrun**, not passed. CI job `tests (feature-on runs)`, step
`Delivery document activation, result decisions, PR operations and environments`,
now selects the module with
`test(/^profiled_results::auto_settlement56::playbook::publication_recovery::/)`
and includes `--test it_profiled`. A CI result must retain its exact source
fingerprint/commit, command, job and outcome. No production database privilege
or credential change is requested.

Follow-up checks actually run (database execution remains unrun):

| Exact command | Outcome |
| --- | --- |
| `cargo test -p aq-delivery --features delivery_playbook --lib --locked` | 116 passed; final source, including nine socket/publication cases and the summary disclosure regression. |
| `cargo test -p aq-supervisor-control --features delivery_playbook,supervisor_full_access --lib retained_publication --locked` | Exit 0; one project-scope test passed, 36 filtered out. |
| `cargo clippy -p aq-server --features delivery_playbook,project_environments --lib --test it_profiled --test it_playbook --locked -- -D warnings` | Exit 0; final source and both integration binaries checked with warnings denied. |
| `cargo clippy -p aq-delivery --features delivery_playbook --lib --locked -- -D warnings` | Exit 0; final delivery source checked with warnings denied. |
| `cargo test -p aq-server --features delivery_playbook,project_environments --test it_playbook --test it_profiled --locked --no-run` | Exit 0 after 16m40s; both integration binaries compiled, no tests executed. Build started before follow-up edits; final-source compilation is checked separately by Clippy. |
| `/tmp/aq-target/debug/deps/it_profiled-9500fd0466812012 publication_recovery:: --list` | Exit 0; nine tests listed, zero benchmarks. Listing is not execution. |
| `cargo fmt --all -- --check` | Exit 0 on final source. Earlier iterations reported differences before `cargo fmt --all` corrected them. |
| `python3 scripts/migration_catalog.py` | Exit 0; 179 contiguous migrations. |

The CI workflow file is outside the Rust/SQL/TOML/lock fingerprint; its SHA256
is `2e36934a04237278a177358c0f36187b05aaeb34cd9cd275c3f94c237a366f9f`.
The final source diff was read against originals saved under `/tmp` because
this workspace has no Git metadata. No `build-budget.toml` edit, commit, push,
deployment or live reconciliation was performed.

### Exact failed-result metadata recovered

The agent registry's supported schema has no `profile` argument on
`result.review.list`; `result.review.show` requires `report_id` and the report
digest, not `result_id` or a source OID. Earlier malformed queries in this
attempt returned HTTP 400 `invalid_request`. Those are not access-denial or
evidence-absence observations.

This bounded project-scoped keyset read (starting immediately before the known
report UUID) succeeded:

```sh
aq run result.review.list --param project_id=a22afccc-d61a-5f77-aef8-9fd73311c68a --param after=4424f991-19cd-4502-95e9-91b50cd20bef --param limit:=1 --json
aq run result.review.show --param project_id=a22afccc-d61a-5f77-aef8-9fd73311c68a --param report_id=4424f991-19cd-4502-95e9-91b50cd20bf0 --param revision=d9fac451948dfaf937b2f5300051cf2b47c3fc20ec18ab67d3812b20ac10b0ec --json
```

Both reads returned the exact report at immutable revision
`d9fac451948dfaf937b2f5300051cf2b47c3fc20ec18ab67d3812b20ac10b0ec`,
project `a22afccc-d61a-5f77-aef8-9fd73311c68a`, repository
`97a459e3-e35f-5af0-85c0-1c095b50519c`, task
`28a8069e-2559-47b5-b433-1eeaffb8e827` and attempt
`a5920827-2b71-5cef-87a4-d20526237ed7`. The show read returned work state
`pushed_ref`, base `2c0b65c38417cfb35322a4c2b68ba21d1496070c`, exact head
`489acd898966ce1d9b07a61b753748ed55779e32`, ref
`refs/heads/aq/28a8069e-2559-47b5-b433-1eeaffb8e827` and provenance digest
`05162408f3c310d88d682600d3784bb9b51579481d18490cda46b790f4c742d1`.
This binds the runner's failed-check input to a report revision; it is separate
from the original b57 report and from this follow-up's tested source fingerprint.

The returned result/work schemas contain no required-check job/test/assertion
fields. The bound check failure remains requested through the existing scoped
access recovery; no missing evidence or broad access restoration is inferred.
The read still reports empty `source_receipt_ids`. It establishes neither live
publisher acceptance nor a released hold. This material read was queued to the
global supervisor as `40891d1f-db19-4328-966f-06ad6867c5c4` (cursor 202).
The final source/check/limit handoff was queued as
`31076f9e-d1ca-450f-a100-b27823d44293` (cursor 203). Queued does not assert
recipient review, task acceptance, source publication or runtime deployment.

To reproduce the source fingerprint from the captured tree:

```sh
python3 - <<'PY'
import hashlib, pathlib, subprocess
paths = sorted(p for p in subprocess.check_output(['rg', '--files', '--hidden'], text=True).splitlines()
               if pathlib.Path(p).suffix in {'.rs', '.sql', '.toml', '.lock'})
h = hashlib.sha256()
for p in paths:
    h.update(p.encode() + b'\0' + hashlib.sha256(pathlib.Path(p).read_bytes()).digest())
print(len(paths), h.hexdigest())
PY
```

## Supplied-tree repair and validation, 2026-11-26

The runner labels this input as retained commit
`8556fd04d03413264f854a4857db1ddd13ac15b9`. There is still no Git metadata,
so that label cannot be independently resolved here. The supplied snapshot
contains 2589 Rust/SQL/TOML/lock files with initial fingerprint
`14d42b876d3e91bcdd92b135eb47b90d190156d507470da2ffca853736e60812`.
The final source fingerprint, using the algorithm above, is
`e5da17b63977afd921ae09fc1d666963186161f1c47a80654b54819d15d1b51d`
(2589 files). The final CI workflow SHA256 is
`1f3cf0ec37029fa1b11c97afe84576ef5cfecf9d84d2120195da21ebcf844d97`.
Earlier sections describe earlier snapshots and checks, not this run.

This follow-up resolves supplied conflict markers in the delivery host imports,
registry tests and CI selector. It preserves both registry tests and both CI
selectors, including the publication recovery module. The obsolete `REFRESHES`
import is removed and the dispatch fence's required `Run` import is retained.

The bridge now recovers an original `already_present` receipt attesting
containment at a later head, preserving that observed OID without attributing
the push to the operation. It still validates the original request's repository,
binding generation, task ref, source commit, remote-head lease and payload hash.
Reconciliation refuses contradictory outcome/resolution responses and normalizes
observation timestamps before returning or retaining them. Inspection returns
only the frozen target's validated Git binding, excluding forge labels. New
socket cases cover containment, invalid/causal acknowledgments, mismatched
operation IDs, contradictory resolutions and a 20,000-digit fractional timestamp.
They assert that no publication or forge request occurs. The classification
matrix additionally exercises `non_fast_forward`, `stale_lease` and `unknown`.

The supplied tree also contains another migration 0179 and a sequence through
0184. The repair's migration is now
`0185_retained_publication_recovery.sql`. Because the normal renumbering CLI
requires missing Git history, this run used the repository script's existing
`rewriter`, `rewrite_lines` and `apply_changes` helpers after asserting that
all supplied migrations except this repair form exactly 0001–0184. A byte
comparison confirms the SQL text changes only `value='179'` to `value='185'`;
the dispatch fences, immutable journals and receipts are preserved. The advisory
lock salt 179 is unchanged. No other task's migration or build budget was edited.
Historical 0179 references and 179-migration check results above remain historical.

All Cargo invocations below set `CARGO_TARGET_DIR=/tmp/aq-target` and
`CARGO_HOME=/tmp/aq-cargo`. Download and helper bytecode caches are under `/tmp`.

| Exact command | Actual outcome in this run |
| --- | --- |
| `cargo test -p aq-delivery --features delivery_playbook --lib github::tests::commands::retained_publication:: --locked` | Initial exit 101 on supplied host conflict markers. After resolving those, exit 101 with 9 passed and the 2 new regressions failing (containment held; timestamp evidence unbounded). After the fixes and expanded classification matrix, exit 0: all 11 passed. |
| `cargo test -p aq-delivery --features delivery_playbook --lib --locked` | Exit 0: 121 passed, including both new regressions. The subsequent classification-only matrix expansion was verified by the 11-test rerun above. |
| `cargo test -p aq-command-registry --features delivery_playbook,supervisor_full_access --lib --locked` | Exit 0: 24 passed, including both tests preserved from the supplied conflict. |
| `cargo test -p aq-supervisor-control --features delivery_playbook,supervisor_full_access --lib retained_publication --locked` | Exit 0: 1 passed, 36 filtered out; own-project inspection/reconciliation paths accepted and cross-project paths refused. |
| `cargo clippy -p aq-delivery -p aq-command-registry --features delivery_playbook,supervisor_full_access --lib --tests --locked -- -D warnings` | Exit 0: changed libraries and tests checked with warnings denied. |
| `cargo fmt --all -- --check` | Exit 0 on the final Rust source. |
| `python3 scripts/migration_catalog.py` | Initially exit 1: duplicate 0179. After renumbering only this repair, exit 0: 185 contiguous migrations. |
| `PYTHONPYCACHEPREFIX=/tmp/aq-python-cache python3 scripts/renumber-migrations.py --check` | Exit 2: no Git repository/merge-base with `origin/main`. Normal main-relative validation remains unrun. |

The inline Python CI selector assertion passed: the resolved workflow retains
`--test it_profiled`, the publication recovery filter and the supplied successor
test filter, and the three conflict files contain no markers. A full YAML parse
was unavailable (`ModuleNotFoundError: yaml`); attempting a temporary parser
installation with `python3 -m pip install --cache-dir /tmp/aq-pip-cache --target
/tmp/aq-yaml PyYAML==6.0.3` also failed because this sandbox has no `pip`.
The workflow diff was read directly. No workflow execution is claimed.

Authenticated evidence was refreshed using the exact command:

```sh
aq run result.review.show --param project_id=a22afccc-d61a-5f77-aef8-9fd73311c68a --param report_id=68d19730-4db8-4668-855b-d3e9572d5050 --param revision=f4b26ac1956b58997c0faca587f675453b20c9fb127da61dbf701b038f2b7b19 --json
```

It again binds that result/revision to repository
`97a459e3-e35f-5af0-85c0-1c095b50519c` and retained commit
`ac8b015d6dfaaa0aa7850d2e7ff13a2d20285cdd`, with empty `source_receipt_ids`.
Separate `aq operation show dfd8cb53-9b5e-833c-bfe8-f32b36408c66 --json`
and `aq operation show de4542bf-b430-8e57-a85e-9eb6f957679d --json` calls
both returned HTTP 404 `not_found` in the separate supervisor journal. Neither
read proves the original publisher accepted or rejected anything. The scoped
`aq commands` registry does not expose the private-journal bridge commands;
runtime classification capability and the actual rejection class remain
unverified. Source fixtures are not deployed capability evidence.

Direct protocol coordination with supervisor
`842211df-4633-4bae-b559-96382fa70912` was refused with `not_found`; the request
to relay/reuse existing inspection task `8cb39922` and the bounded access
findings were queued to global conversation
`ea5785f9-3da2-4bf4-8f64-e9558147b2f3` as message
`81170658-8397-4191-b704-995b512a3858`, cursor 239. No duplicate repair was created.

`DATABASE_URL` remains unset. Database integration execution, migration
application, full-workspace/server qualification, CodeScene and main-relative
build budgets were not run. Release requires normal review/CI, a numbering
check against current main, and operator-provided access through the existing
authenticated owner/project-supervisor bridge to the **original** AQ Git private
socket/journal. The runtime must independently advertise
`publication_rejection_classification_v1` to support classification. Any live
observation-only reconciliation needs a separate decision bound to the original
operation, repository/binding generation and exact retained result/revision.
The actual refusal may require an additional narrow operator policy decision;
the available evidence cannot identify it. No live reconciliation, redispatch,
worker retry, permission/credential/host change, hold release, merge, deployment,
commit or push occurred. The source diff was read against saved originals in
`/tmp` because Git metadata is absent.

The final source/check/limit handoff was queued to the same global conversation
as message `07aa6dbc-36b6-4f89-8c0e-3b20e3d9aad8`, cursor 240. This records
mailbox delivery only; it does not assert recipient review or task acceptance.
