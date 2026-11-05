# F1b: supervisor conversation dispatch, reply and scoped supervisor credential

Task `steady-harbor-87.2` (parent `steady-harbor-87`). Plan:
`docs/superpowers/plans/2026-11-03-f1b-supervisor-dispatch-reply.md` (commit
`0b6b012f`). Implementation commit: `4efb2e62` (squashed), followed by the final
`git merge origin/main` recorded below.

## Acceptance

| Acceptance item | Evidence |
| --- | --- |
| An owner message is dispatched to the live F1a supervisor session as one bounded turn of 1–16 refs; the plan carries no bodies or credentials | Steps 1–7 of the acceptance test: the owner POSTs through `routes`, the mail is reserved, `prepare_turn` records the turn, and the test asserts that the serialized `CentralPlan` contains no message body. The F1a host starts with `turn.input` as its exact startup bytes, and the staged file equals those bytes |
| Dispatch intent and host-input intent are durable before handoff; Running is not delivery; uncertain stays `delivery_unknown`; no automatic re-send | `central_supervisor_dispatches` (migration 0048) stores the dispatch, operation ids, batch digest and exact input bytes before the plan is authorized (`dispatch.rs:80-167`). `authorize_dispatch` (`dispatch.rs:170-205`) binds plan, batch and startup digest to that row. History reports `pending` while the session is Running and before the handoff (test step 8). `Supervisor::handoff` (`aq-supervisor-host/src/lib.rs:466`) re-reads the staged input and journals a verified `handoff` host fact. On a mismatch it reports `Unknown`, which surfaces as `delivery_unknown` (`dispatch.rs:229-239`). Nothing re-sends: one turn per reservation and per generation (`UNIQUE` constraints) |
| The reply goes through the mailbox owners, with a server-derived author, an exact dispatch association, and separate logical admission, host delivery and receipt | `POST /v1/supervisor/conversation/replies` (`global.rs:130`, `reply.rs`). The author comes only from the credential's `SupervisorPrincipal`. The request must name the exact `dispatch_id`/`host_operation_id` of the active generation's turn: a wrong dispatch returns 409, and a replay returns the identical receipt. History shows the supervisor item with its `dispatch` ref and `host_delivery: not_attempted`, separate from the owner item's `delivered` (test steps 12–14; schema `protocol/human-supervisor-conversation/v1/schema.json` `SupervisorItem`) |
| One scoped central credential (graph.read, graph.write, enqueue, messaging, decisions), bound to the current generation, expiring/revoked with it, stored only in the supervisor host, never reaching a worker, with no fabricated worker/task/attempt identity | `reply::issue_credential`/`authenticate` (`reply.rs:65`, `reply.rs:118`): only the sha256 digest is stored, the row is generation-FK'd, and issuing revokes prior live rows (revoke-only trigger). Every use checks that the generation is still the actor's active head. `Supervisor::install_credential` (`lib.rs:521`) writes it `0600`, `create_new`, under the session launch directory. The principal type has no worker, task or attempt fields. The routes live in the worker router but accept only the bearer supervisor credential, so owner headers alone return 401. After `stop`, the same token returns 401. The plaintext token is absent from the credentials, host facts and mailbox tables (test steps 10–11, 15–16) |
| `supervisor.conversation` capability true only when dispatch and reply are live | `dashboard_api/service.rs:172` gates the capability on `dispatch::live`, which is true only while some actor's head generation is StartCommitted/RunningReported. The test asserts false before start, true while running and false after stop |
| `cargo test --locked --workspace owner_message_reaches_supervisor_and_reply_is_recorded` passes, matching ≥1 test | `F1b-evidence/acceptance.txt`: `test owner_message_reaches_supervisor_and_reply_is_recorded ... ok` and `test result: ok. 1 passed; 0 failed; ... 11 filtered out`. Every other test binary in the workspace reports 0 passed, so exactly one test matched |

## Commands and environment

All checks ran through AQ's pytest resource gate:

```
F1B_CHECK=<check> DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
  aq test docs/implementation/F1b-evidence/test_cargo.py -q --timeout=1800
```

The wrapper sets `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0
CARGO_INCREMENTAL=0 RUST_TEST_THREADS=2` and writes the cargo output to
`F1b-evidence/<check>.txt`. The database is the disposable `agent-q-test-db`
container from `scripts/test-db`. The commands are listed in `test_cargo.py`:

| Check | Command | Result |
| --- | --- | --- |
| acceptance | `cargo test --locked --workspace owner_message_reaches_supervisor_and_reply_is_recorded -- --exact --include-ignored` | 1 passed (owner_message_reaches_supervisor_and_reply_is_recorded); all other binaries 0 matched |
| acceptance_host | `cargo test --locked -p aq-supervisor-host --test supervisor_session owner_message_reaches_supervisor_and_reply_is_recorded` | 1 passed, 11 filtered out |
| host | `cargo test --locked -p aq-supervisor-host --test supervisor_session -- --test-threads=1` | 12 passed, 0 failed (F1a lifecycle regressions plus acceptance) |
| conversation | `cargo test --locked -p aq-server --features test-support --test supervisor_conversation` | 6 passed, 0 failed (existing API-07 conversation tests, schema change compatible) |
| central37 | `cargo test --locked -p aq-server --features test-support central37` | 7 passed, 0 failed |
| format_version | `cargo test --locked -p aq-server --features test-support --lib state_format_is_the_latest_migration` | 1 passed (state format 48 = latest migration) |
| format | `cargo fmt --check` | clean (empty output) |
| clippy | `cargo clippy --locked -p aq-server -p aq-supervisor-host --features aq-server/test-support --all-targets -- -D warnings` | clean, `-D warnings` |

`-- --exact --include-ignored` only narrows the filter. The test is not
`#[ignore]`d, so the literal acceptance command selects the same single test.

Test validity: changing `delivery()`'s `HandedOff` arm to `"pending"` made the
acceptance test fail at `supervisor_session.rs:886` (left `"pending"`, right
`"delivered"`). The mutation was reverted before commit.

## Rulings

- **One consolidated acceptance test.** It is the named deliverable. Its
  mutation-checked assertions cover every acceptance row instead of separate
  RED unit tests per helper. Cost if wrong: weaker localization of future
  regressions.
- **The capability is checked through `dispatch::live`,** the exact term the
  dashboard capability uses, not through a full dashboard API round trip.
  Cost if wrong: a wiring regression in `service.rs` would go unseen by this
  test.
- **Handoff proof re-reads `context-0.bin`** and compares its digest with the
  recorded input. A mismatch becomes `Unknown` (delivery_unknown) and is never
  re-sent.
- **`aq-central-authority/src/owner.rs` changed outside the plan:** its
  `SET LOCAL` statements use `sqlx::query`, and `inspect`/`generation` return
  boxed `Send` futures, so owner reads can run inside Axum handlers.
- **History schema `maxItems` was raised to 80** (from 16 history items;
  since reverted to 16 by `steady-harbor-87.2.4`), because up to 64 supervisor replies interleave with the owner items. This
  is a protocol addition and needs a matching dashboard consumer change
  (cross-repo follow-up comment).
- **No adversarial review:** the task is not marked for one (subtask 6 is N/A).

## Known limits (follow-ups)

- One turn per generation (`UNIQUE(actor_id, generation)`): a second owner
  message needs a successor generation. Reservation consumption after a turn
  is not implemented.
- Credential revocation is check-based: the credential is unusable once the
  generation is no longer the active head, because every use re-checks this.
  Only re-issue writes `revoked_at_ms`; stop does not. There is a TOCTOU
  window between `authenticate` and the reply write.
- Only the `messaging` scope is exercised. The `graph.read`, `graph.write`,
  `enqueue` and `decisions` scopes are issued but not yet enforced by any
  route (F2/F5 own those commands).
- ~~History anchors replies at the batch's max cursor.~~ ~~`inspect` takes a
  `FOR UPDATE` row lock.~~ Both are fixed by `steady-harbor-87.2.4`
  (`F1b-reply-cursoring.md`): replies have their own `A.R` cursor, pagination
  counts them, `maxItems` is back to 16, and `inspect` reads a lock-free
  snapshot.

  message needs a successor generation. A completed turn releases its
  reservation; see [F1b-reservation-consumption.md](F1b-reservation-consumption.md).
- Credential lifecycle and authenticate/write races are addressed by
  `steady-harbor-87.2.3`; see [F1b-scopes.md](F1b-scopes.md) for explicit
  transactional revocation and the reply write check.
- Only the `messaging` route exists at this checkpoint. All five issued
  scopes are checked by the shared authenticator and tested in the scope
  follow-up; [F1b-scopes.md](F1b-scopes.md) defines the required integration
  for F2/F5 routes when those features land.
- History anchors replies at the batch's max cursor. Reply-level cursoring
  and pagination are not separate.
- `inspect` takes a `FOR UPDATE` row lock, including from the `live` read
  path.
- The `aq-supervisor-host` binary (`main.rs`) does not yet call
  `handoff`/`install_credential` or carry the credential to the session
  daemon wiring. The library API and the test drive them directly.
- Not rebuilt or qualified: no image rebuild and no full-suite or provider
  run.

The first `conversation` run used a bare `supervisor_conversation` name filter,
which matched zero tests (test names do not contain the file name). It was
corrected to `--test supervisor_conversation` and rerun. Only the corrected
run counts.

## Follow-ups

- `steady-harbor-87.2.1`: reservation consumption and successor turns.
- `steady-harbor-87.2.2`: wire `handoff`/`install_credential` into the
  `aq-supervisor-host` binary.
- `steady-harbor-87.2.3`: enforce the graph, enqueue and decisions scopes,
  revoke on stop, and close the authenticate/write TOCTOU window.
- `steady-harbor-87.2.4`: reply cursoring, and no `FOR UPDATE` on the
  `live` path.
- The `CROSS-REPO FOLLOW-UP (aq-dashboard)` task comment covers the history
  schema additions.

## Final merge

`git merge origin/main` (merge `ed37b6dd`, main `c753a633`) brought only
compose/deploy/docs changes: no Rust, migration or Dockerfile changes. After
the merge, the `acceptance` check was rerun and
`F1b-evidence/acceptance.txt` now holds that post-merge run:
`owner_message_reaches_supervisor_and_reply_is_recorded ... ok`, 1 passed and 0
failed, with every other test binary matching 0 tests.

## Typed History attachments (wise-apex-23, for G8)

Supervisor replies may attach up to four typed items, which History serves on
the supervisor item: `proposed_task_draft` (project and revision pins and a
canonical payload digest) and `action_receipt` (a confirmed F2 `task.create` or
`task.graph.create` operation of the same supervisor session, with created
task/edge IDs and receipt refs). Each item carries its reply's `message_id`. The
server resolves and snapshots items in the reply transaction, in
`central_supervisor_replies.attachments` (state format 62, migration 0062), so
receipt access survives retirement and credential rotation. Unknown, pending,
retired or other-principal operations are 409 before any write. Cursoring is
unchanged. The protocol change is additive. The README publishes the schema
digest, and `schema.sha256` pins it.

Evidence: `supervisor_turns`
`reply_attachments_serve_typed_drafts_and_session_receipts_through_history` and
`attachment_fixtures_match_schema_and_published_digest`.
Commands (logs in `F1b-attachments-evidence/`, run through
`CHECKS=<list> aq test docs/implementation/F1b-attachments-evidence/test_cargo.py -n 0`
with the disposable PostgreSQL on 127.0.0.1:55432): `turns` (supervisor_turns:
7 passed, including the pre-upgrade format-48 reply tests, which still pass
because replies without attachments write the pre-0062 row), `conversation`
(6 passed), `reply_lib` (`--lib messaging::global`: 17 passed),
`format_version` (1 passed, state format 62), and `clippy -D warnings` and
`fmt --check` (both clean).
