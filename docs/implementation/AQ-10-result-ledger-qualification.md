# Native result ledger component checkpoint

Source `c1ab603` implements bounded report recording and correction history.
Seven actual disposable PostgreSQL/HTTP tests pass, plus all six Started and eight
Stopped regressions. Strict server all-target/all-feature Clippy and formatting
pass. Exact source/log hashes are in
[AQ-10-result-ledger-qualification.json](AQ-10-result-ledger-qualification.json).

Migration024 is deliberately not registered yet. These component tests apply its
SQL explicitly to disposable databases; ordered migration and assembled daemon
qualification must follow integration of022/023. No placeholder migration is used.
The candidate route must not be deployed without its registered schema.

The owner checks the current machine credential under lock, original admitted
runtime, actual Started/Stopped records and the exact retained typed Outcome
artifact linked to the sealed launch. Retained definitions are verified against
the original execution spec; expiry of their admission observation does not rewrite
history or prevent recording a stopped worker's claim. Current task/input revisions
and evidence applicability belong to later eligibility evaluation.

The immutable receipt says Recorded. Report IDs/operation IDs cannot be reused
with changed bytes. Corrections must supersede the current head; concurrent
corrections produce one winner. Each attempt keeps at most64 reports and4MiB of
report bodies. Exact historical replay still works at capacity. SQL guards bind
the actual stop/outcome owners and require report/head to commit atomically.

Tests cover missing owner records, mismatched stop evidence, cancellation and
retired new-launch configuration, quota/replay, correction races, immutable rows,
direct SQL forgery and missing-head rollback, and credential rotation during an
observed transaction lock wait. HTTP uses current machine authentication, a64KiB
body limit, no-store responses and general work admission. Reporting cannot consume
the reserved control connection pool. Task/source/effect/graph state is unchanged.

This checkpoint does not verify source-receipt claims, discover pending effects,
accept review/check evidence, establish eligibility, release task/resource claims,
or complete graph work. Host/launch fixture inputs are explicitly synthetic;
the actual daemon owners are exercised, not a paid provider or native worker.
