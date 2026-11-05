# F1b reservation consumption (steady-harbor-87.2.1)

A recorded supervisor reply or resolved `not_handed_off` result now consumes the
turn's reservation and clears the actor's current reservation. Both changes occur
inside the transaction recording the terminal evidence, via migration 49's reply
and generation triggers. Repeated terminal reports and reply operation replays
are idempotent and cannot clear a later reservation. Migration 49 also consumes
already recorded terminal turns when upgrading state 48.

`consumed` is a terminal reservation state (revision 2, no cancellation operation).
The original reserve operation and its receipt remain immutable; lookup returns
that original receipt alongside the current consumed reservation view. Consumption
uses the existing terminal evidence instead of fabricating an Owner operation or
changing the Owner-operation revision/budget sequence. Original dispatches, mail
members and all-generation central session history remain retained. The new state
is included in the reservation schema, and startup validates the updated guards,
checks, consumption triggers and consumed evidence while preserving historical
state-37 validation.

## Turn model decision

Keep `UNIQUE(actor_id,generation)`: each `CentralPlan`/`Run` and physical host
session contains one immutable startup turn. A second batch uses a new reservation
and successor generation; it still requires stopped-writer evidence for its
predecessor. Supporting multiple turns in a generation would require new host
input and authority/reply association semantics, beyond releasing reservations.
The database tests exercise generation 1 then generation 2 through the real
reservation, dispatch, central session and reply owners.

Unknown, pending, and merely Running deliveries do not consume reservations.
A reply can complete a reservation while its host delivery stays unknown; the
exact original association remains unknown. Automatic batch selection skips all
consumed mail, including known failures, so consumption never creates an automatic
resend. A dispatched reservation cannot use pre-plan cancellation to release an
uncertain turn. No provider, host process, or sleep is added to these tests.

## Verification

Run through `aq test` with the existing default worker cap and markers:

```sh
export DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres
F1B_CONSUMPTION_CHECK=turns aq test docs/implementation/F1b-consumption-evidence/test_cargo.py -q
F1B_CONSUMPTION_CHECK=area aq test docs/implementation/F1b-consumption-evidence/test_cargo.py -q
F1B_CONSUMPTION_CHECK=reservations aq test docs/implementation/F1b-consumption-evidence/test_cargo.py -q
F1B_CONSUMPTION_CHECK=related aq test docs/implementation/F1b-consumption-evidence/test_cargo.py -q
F1B_CONSUMPTION_CHECK=version aq test docs/implementation/F1b-consumption-evidence/test_cargo.py -q
cargo fmt -p aq-server --check
git diff --check
```

The adapter names the exact Cargo commands and writes each complete log beside
itself. The slot's Cargo target is private; build jobs are capped at two, debug
info and incremental compilation are disabled. Passing results:

| Check | Rust tests passed |
| --- | ---: |
| Supervisor turn consumption and upgrades | 5 |
| Historical startup/upgrade guards | 7 |
| Reservation owner | 12 |
| Central sessions | 9 |
| Supervisor conversation | 6 |
| Latest migration matches state format | 1 |

`cargo fmt -p aq-server --check` and `git diff --check` also pass. The two
`turns-*-failure.txt` logs retain corrected initial compile failures; `turns.txt`
records the successful run. Repository-wide formatting could not discover the
vendored workspace from this nested worktree; package-scoped formatting succeeds.

This follow-up requires parent F1b commit `baf26451`, merged into the assigned
branch because its dispatch/reply files were absent at the assigned source base
`e84129d9`. The parent implementation and merge ancestry remain intact.
The final merge of `origin/main` at `b807f893` reconciles the parent's squashed
implementation with that prerequisite history. Its runtime code is identical to
`baf26451`; the corrected parent evidence adapter and new evidence are retained.
The five turn-consumption tests pass again after this merge resolution.
