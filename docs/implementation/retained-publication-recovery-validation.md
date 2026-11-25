# Retained publication recovery: source validation

This is source/test evidence for task `28a8069e-2559-47b5-b433-1eeaffb8e827`,
not a release, live reconciliation, result acceptance or hold override.
The sandbox has no Git metadata, so a Git HEAD cannot be reported. The exact
code snapshot fingerprint (2542 source/configuration files) is
`e910faf1916fd620f8b4d734c124a5fe8c95ed59b8f7b7337992a16aa7579a63`. It covers
all sorted paths from `rg --files --hidden` ending in `.rs`, `.sql`, `.toml`
or `.lock`, hashing each as `path UTF-8 + NUL + SHA256(file bytes)` and hashing
the concatenation with SHA256. Documentation is outside that code fingerprint.

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
