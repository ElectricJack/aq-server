# G6 follow-up: Results feedback read revision 1.1

Task `keen-current-13`, branch `aq/keen-current-13`, source base
`841e19ff6260ce7a97c600227e94c3c42c1abec5`.

The dashboard can restore the direct follow toggle, retained feedback task links,
and explicit thread resolution after reload. Existing Results routes now expose
additive read revision `results-feedback-1.1`; envelopes remain
`dashboard-read-1` and the seven G1 write operations are unchanged.

`ResultProjection.directly_following` reads only the current principal's explicit
preference on the stable Result. Thread opt-in continues to affect Following
membership independently. Comment `feedback_links` expose operation/project/task
IDs and exact retained result/artifact/thread/comment revision origins; corrections
do not replace those origins. Comment `resolution` exposes the resolving principal,
reason, stable follow-up Result/revision and exact report ID. Open threads return
null. Links neither submit follow-ups nor resolve threads.

The existing `revision.addressed_feedback` field now reads sorted thread UUIDs
from explicit resolutions pinned to the exact report UUID and digest. It is a
mutable supplemental compatibility projection, not native submitted content or
part of its digest. Assessment observation includes this array, so resolving an
older thread against a newer revision advances the newer assessment on refresh.
Acceptance remains `unreviewed`.

## Authorized boundary and follow-up

Supervisor message `msg-95b00ab29ecc4e46a2b59dd63669034a` authorized the bounded
implementation after inspection confirmed no authoritative submission record.
Both detail and comment pages expose
`follow_up_submission: {"state":"unsupported"}`. This is unavailable capability,
not an observation that the user has not submitted. It remains independent of
creation/linking and explicit resolution. No submission is inferred from either.

**Named follow-up: independent follow-up submission owner/operation.** The
supervisor will file this from the close summary. That future owner must retain
submission against an exact revision independently of task creation, linking,
resolution and acceptance. This task adds no write operation or SQL migration.

The same supervisor message waives absent `origin/dev` and authorizes merging
fetched `origin/main` for final synchronization. The task remains in direct mode;
AQ owns integration and no worker push targets `main`.

## Contract and compatibility

`protocol/results-feedback/v1/README.md` defines the new fields and the absence
semantics for original v1 responses. The generated schema keeps legacy fields and
adds defaults for decoding v1. `schema-1.0.json` preserves the original strict
schema, SHA-256 pinned by the compatibility fixture. New readers accept old
responses. Tolerant old readers can ignore the documented additions; strict old
readers that reject unknown fields require a decoder update. The compatibility
check explicitly projects only the documented additions before checking the
frozen schema; it does not claim strict v1 readers accept raw v1.1 objects.

Valid fixtures cover resolved and open linked threads, corrected comments with
retained historical origins, exact resolution and unsupported submission.
Invalid fixtures reject wrong direct-follow types, invented read versions or
submission states, inexact resolution digests, numeric comment revisions and
malformed artifact digests. Replay fixtures preserve the same read state across
identical replay and reload. Rust decoding tests cover old and current fixtures.

## Verification

Checks run in the foreground through AQ's pytest admission, using a single
serial adapter at `.aq/tmp/g6-followup/test_checks.py`. Cargo build jobs, Rust
test threads and Tokio workers are each limited to two. PostgreSQL is the existing
disposable test service at `127.0.0.1:55432`. The Graph fixture is an independently
built binary copied into the slot, pinned at SHA-256
`5d649f1a3dbcc6f1acd1b18b0a6c2a8e17eefa48cbadc43bc35083eaaae5dfd9`.
No sibling source dependency is added. Its child uses an isolated database/token,
clears inherited environment and limits its pool and Tokio threads to two.

Focused runtime coverage includes real Graph create/link/recovery and replay,
correction history, links isolated to the exact comment/revision, replies without
inherited links, a rebuilt router for reload, thread-only Following membership,
explicit follow/unfollow while membership persists, another principal's preference,
unknown follow-up rejection, changed replay rejection, exact old/new revision
resolution, assessment version refresh and unchanged acceptance.

Logs are retained in `docs/implementation/evidence/G6-followup/`.

The wire phase passed through
`G6_FOLLOWUP_PHASE=wire aq test .aq/tmp/g6-followup/test_checks.py -q`
(one serial adapter, 4.16 seconds):

| Exact subprocess command | Result / evidence log |
| --- | --- |
| `python protocol/results-feedback/v1/check.py` | Passed valid, invalid, replay, legacy and additive compatibility; `wire-fixtures.log` |
| `python protocol/dashboard-actions/v1/check.py` | Passed; `action-fixtures.log` |
| `python protocol/dashboard-read/v1/check.py` | Five fixtures passed; `read-fixtures.log` |

Schema generation passed with
`cargo run --locked -p aq-server --example results_feedback_contract`
(`schema-generate.log`). The required focused rerun passed through
`G6_FOLLOWUP_PHASE=retry aq test .aq/tmp/g6-followup/test_checks.py -q`
(one serial adapter, 97.72 seconds). The area rerun passed through
`G6_FOLLOWUP_PHASE=area-retry aq test .aq/tmp/g6-followup/test_checks.py -q`
(one serial adapter, 16.21 seconds).

| Exact subprocess command | Result / evidence log |
| --- | --- |
| `cargo test --locked --workspace results_feedback` | Passed: one decoder unit test and nine Results integration tests; `results-feedback-recheck.log` |
| `cargo fmt --check` | Passed; `fmt.log`; rechecked after the reply precondition correction |
| `cargo clippy --locked -p aq-server --all-targets -- -D warnings` | Passed; `clippy.log` |
| `cargo run --locked -p aq-server --example results_feedback_contract -- --check` | Passed; `schema-check.log` |
| `cargo test --locked -p aq-server --test dashboard_read --test dashboard_actions --test dashboard_mounts` | Rerun passed: 8 action, 3 mount, 6 read tests; `dashboard-area-recheck.log` |

The initial focused run (`results-feedback.log`) caught a stale Result version in
an added reply step. The scenario now fetches the current Result version before
posting the reply; backend behavior and assertions were preserved. The initial
area run (`dashboard-area.log`) failed the unchanged
`acl_expiry_during_database_wait_refuses_read` test with HTTP 200 instead of 403.
Its 50 ms scheduling assumption can allow authorization to start after the test
clock advances (inferred from the unchanged auth query and the capability-empty
response). The unchanged area suite passed on rerun; the initial failure remains
visible rather than being represented as a clean first pass.

The retained `validation.py` adapter can also be invoked by its committed path.
For runtime phases, supply `AQ_TEST_GRAPH_BINARY` and `AQ_TEST_GRAPH_SHA256` for
an independently built Graph binary when the slot-local copy is unavailable;
the adapter verifies its digest before use. `DATABASE_URL` can select another
disposable PostgreSQL service. Fixture-only phases do not require Graph.

## Final synchronization

`aq git pull --branch main --json` fetched and merged
`origin/main` at `ea28da1ca62184e9f5f945b73b62320edbe903e0` into the task
branch without conflicts. Explicit `git merge origin/main` then reported already
up to date. The single implementation commit remains a parent of the merge;
no default-branch write was performed.

The fetched main already contains a deterministic ACL test wait replacing the
50 ms delay (`crates/aq-server/tests/dashboard_read.rs` in `ea28da1c`). This
addresses the timing assumption observed in the earlier area run and is retained
through the merge, without a separate change from this task.

The first post-merge compatibility adapter caught stale copied dashboard-read
schema digests in the action fixture (`action-merged.log`). The incoming schema
changed to SHA-256 `9d7cac3073706afa942033b8f7975588285f1b47fb92db47b764c40348f07801`.
Only the copied digest metadata was refreshed in action and Results compatibility
fixtures; the Results checker also now verifies those copied digests. No action
contract or production behavior changed. Results, action and dashboard-read
fixture checks then passed against the fetched schemas (`wire-merged-recheck.log`,
`action-merged-recheck.log`, `read-merged-recheck.log`).

Post-merge compatibility and area admission passed through
`G6_FOLLOWUP_PHASE=merged-compat-retry aq test .aq/tmp/g6-followup/test_checks.py -q`
(one serial adapter, 225.95 seconds, including Cargo lock/build time). Results,
action and read fixtures passed; the dashboard area suite passed all 18 tests
(8 action, 3 mount and 7 read), including the deterministic ACL wait test.

The focused post-merge runtime checks use
`G6_FOLLOWUP_PHASE=merged aq test .aq/tmp/g6-followup/test_checks.py -q`
(one serial adapter, 676.80 seconds, including Cargo lock/build time).
All targeted merged checks passed:

| Exact subprocess command | Result / evidence log |
| --- | --- |
| `cargo test --locked --workspace --test profiled_results results_feedback` | Nine passed; `results-feedback-merged.log` |
| `cargo test --locked -p aq-server --lib results_feedback` | One legacy/current decoder test passed; `decoder-merged.log` |
| `cargo run --locked -p aq-server --example results_feedback_contract -- --check` | Passed with no drift; `schema-merged.log` |

The final branch publication OID and AQ close/integration result are recorded in
the task's closing evidence. The independent follow-up submission owner/operation
remains the named, supervisor-owned follow-up described above.
