# AQ-10 stopped-host owner qualification

Daemon source6da51c8/state21 passed694 workspace tests with all features,
6ignored, strict workspace/all-target/all-feature lint and formatting. Supplemental
test-only sourced0f5884 observes actual blocked SQL transactions before rotating
credentials or advancing time;8 stopped-owner tests and strict targeted lint pass.
Earlier default-feature618/6ignored results are retained separately, not added.

The exact aq-admission0.1.4 archive SHA256 is
`4cf2395d8e50e6beaf92ee4fd1e3ea2116a54937dab9520ad681e7f3c4acc50e`.
Independent extraction passes28 tests and strict lint. Public0.1.0 dependencies,
consumer source/lock and all qualification logs are retained in the [manifest](AQ-10-stopped-owner-qualification.json).

Actual synthetic tmux host source14354e7 produced a Running observation and opaque
StoppedProof. The extracted contract decoded them, verified the full original
physical guardian/pane linkage and round-tripped the report. This is **wire
serialization compatibility only**. That worker output candidate is explicitly
unqualified: a sole SIGSTOPped control observer can block PTY output. Reader-lease
work remains pending. No live-provider or full worker-journal qualification is
inferred from this compatibility check.

The owner accepts valid original-host evidence after cancellation/expiry and
launch-policy retirement, records stop/receipt/journal atomically and preserves
all task/source/effect holds. Exact replay is historical; changed proof or operation
reuse refuses. No result acceptance, resource release or graph completion occurs.
The [implementation plan](AQ-10-result-settlement-plan.md) describes the later
stages and changed-incarnation historical recovery requirement.
