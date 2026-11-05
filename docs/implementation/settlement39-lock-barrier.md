# Settlement39 authorization-lock observation

Task: `steady-cascade-45.15`.

The reported G6 baseline was 66 Results cases passing and two failing at the
400 ms authorization-lock observer; a serial settlement recheck still failed
the concurrent-cancellation case. The referenced G6 document/logs are absent
from this task's source base (`d6ed2431`), whose Results target has 60 cases.
The task description is the retained failure baseline. A diagnostic execution
of the cancellation case passed here, consistent with the reported intermittency.

## Lock ordering and observation

`Service::evaluate` observes the requester without a row lock, reads the original
owner state and current Graph/Git observations, then opens its final transaction.
That transaction acquires the settlement budget and common node/attempt/stop
authority before the combined decision owner's approver authorization lock.
After applicability verification it fences the distinct requester identity.

First completion dispatch reloads common authority, fences the requester, then
checks the retained completion/effect/applicability basis and the combined
decision's approver. Native cancellation takes the same node lock before it
checks whether an evaluation was already accepted. These are actual SQL owner
locks; the test peers remain synthetic Graph/Git/launch peers.

The old observer started its 400 ms timer immediately after spawning the entire
evaluation/completion operation. That timer included all preliminary owner reads,
external observations and review validation. No production contract promises
that this work reaches the authorization query in 400 ms. The query itself
matches the current authorization SQL, but a database-wide `LIKE` match did not
prove which transaction caused the wait. The cancellation observer similarly
matched any node-lock waiter in the fixture database.

The replacement barrier records the backend PID of the transaction holding the
specific user row. It requires the exact authorization SQL, a live PostgreSQL
lock wait and that PID in `pg_blocking_pids(waiter_pid)`. It returns the waiting
backend PID. The cancellation test then proves the second blocking edge from
the cancellation node query to that exact acceptance backend before releasing
the user row.

The barrier races observation against the live operation's join handle. An
operation returning, failing or panicking before observation immediately fails
the test with its result, query and blocker PID. Preliminary work is governed
by the operation's existing bounded I/O, while a real SQL lock timeout still
fails the test. The independent observer startup timer is removed; production
SQL/network deadlines, lock order and authorization/freshness checks are intact.

## Preserved qualification

The existing assertions still require an accepted evaluation to survive
concurrent native cancellation, one confirmed logical Graph write, zero
evaluation rows after requester expiry/backward time, and zero first dispatches
or Graph writes after approver expiry. Clock changes and lock release occur only
after the exact blocking edge is observed. No production source or schema changes.

Validation runs through the foreground `aq test` semaphore using a temporary
pytest bridge, with `CARGO_BUILD_JOBS=1`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_INCREMENTAL=0` and one Rust test thread. The bridge runs:

```sh
cargo test --locked --offline -p aq-server --test profiled_results settlement_owner39 -- --test-threads=1 --nocapture
cargo test --locked --offline -p aq-server --test profiled_results -- --test-threads=1 --nocapture
rustfmt --edition 2024 --check --config skip_children=true crates/aq-server/tests/profiled_results/settlement_owner39.rs
git diff --check
```

Test results and final publication evidence are recorded in the task comments.
