# F1b follow-up: reply cursoring and a lock-free `inspect`

Task `steady-harbor-87.2.4` (parent `steady-harbor-87.2`, see `F1b.md`).

## What changed

| Item | Change |
| --- | --- |
| Replies get their own cursor | `reads.rs:155-174`: owner items keep cursor `N` (their mailbox cursor) at position `(N, 0)`. A supervisor reply is at `(A, R)` and is rendered `"A.R"`. `A` is the max cursor of the batch it answers (the old anchor). `R` (≥ 1) is the reply's own cursor in the reply mailbox. Replies therefore sort after the owner message they answer and before the next one. No migration is needed: both numbers already exist |
| Exact merged pagination | `history` (`reads.rs:176-357`) filters both streams by the same row comparison (`(cursor,0) > / < (a,r)` for owners, `(anchor,cursor) > / < (a,r)` for replies). It fetches `limit+1` items from each stream, merges them in page order (`reads.rs:260`), then applies `limit`, the 256 KiB budget, `has_more` and `next_cursor` across the merged items. `before`/`after` accept either cursor form, and `after` still accepts `0`. The old fixed `LIMIT 64` reply side-query is gone, and a reply is never dropped or repeated across pages |
| Schema back to 16 | `schema.json`: History `items.maxItems` goes back from 80 to 16. `SupervisorItem.cursor` is `^[1-9][0-9]{0,18}\.[1-9][0-9]{0,18}$`. `next_cursor` accepts either form. `through_cursor` and `HumanItem.cursor` are unchanged. The README documents the cursor forms |
| Schema checker | `aq-agent-api` `check` matches a closed set of patterns. It gains the two new cursor patterns plus `reply_cursor_pattern_tests` |
| `inspect` is lock-free | `owner.rs:110-145`: `read()` (behind `inspect` and `generation`) runs in a `REPEATABLE READ, READ ONLY` transaction (`begin_read`), and `read_head(.., lock=false)` has no `FOR UPDATE`. Writers (prepare/start/renew/report/`current`) keep the locking `head()`. The `dispatch::live` capability probe and credential checks no longer wait on, or block, a writer |

## Tests (RED then GREEN)

- `aq-server/tests/central_sessions.rs:543`
  `inspect_reads_without_locking_the_session_head`: a writer holds the head row
  `FOR UPDATE`, and `inspect`/`generation` must still return the snapshot
  within 1 s.
  - RED (`inspect.red.txt`): `inspect must not wait on the head row lock: Elapsed(())`.
  - GREEN: 10 passed.
- `aq-supervisor-host/tests/supervisor_session.rs` acceptance step 8b
  (`:1071`) and `assert_history_pages_exactly` (`:701`): 17 more replies to the
  same turn (19 items in total). The test pages forward from `after=0` and
  backward from the newest page, with limits 1, 5 and 16. It asserts every
  page has between 1 and `limit` items and `next_cursor` is the boundary item.
  It asserts the pages concatenate to the exact order (owner, then the 18
  replies in reply order) with distinct cursors, a mid-stream `after=<reply cursor>`
  resumes at the next item, and every page validates against the public schema.
  - RED (`acceptance_host.red.txt`): `items.len() <= limit` failed at limit 1,
    because the owner item plus its interleaved replies came back as one page.
  - GREEN: 1 passed.

## Commands

```
F1B4_CHECK=<check> DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
  aq test docs/implementation/F1b-cursoring-evidence/test_cargo.py -q --timeout=1800
```

The commands are in `F1b-cursoring-evidence/test_cargo.py`, and the logs are
`F1b-cursoring-evidence/<check>.txt`.

| Check | Command | Result |
| --- | --- | --- |
| inspect | `cargo test --locked -p aq-server --features test-support --test central_sessions` | 10 passed, 0 failed (RED before: `inspect.red.txt`) |
| acceptance_host | `cargo test --locked -p aq-supervisor-host --test supervisor_session owner_message_reaches_supervisor_and_reply_is_recorded` | 1 passed, 11 filtered out (RED before: `acceptance_host.red.txt`) |
| api | `cargo test --locked -p aq-agent-api --lib pattern_tests` | 2 passed |
| conversation | `cargo test --locked -p aq-server --features test-support --test supervisor_conversation` | 6 passed, 0 failed (API-07 history contract unchanged for owner items) |
| host | `cargo test --locked -p aq-supervisor-host --test supervisor_session -- --test-threads=1` | 12 passed, 0 failed |
| central37 | `cargo test --locked -p aq-server --features test-support central37` | 7 passed, 0 failed |
| format | `cargo fmt --check` | clean |
| clippy | `cargo clippy --locked -p aq-server -p aq-supervisor-host -p aq-central-authority -p aq-agent-api --features aq-server/test-support --all-targets -- -D warnings` | clean |

No migration was added (operator note: 0050 is reserved for B2).

## Rulings

- **A composite read-side cursor instead of a new sequence.** Allocating
  reply cursors from the owner mailbox head, or from a conversation-wide
  sequence, needs a migration and changes the writer. `(anchor, reply cursor)`
  is total, stable and derived from existing rows. Cost if wrong: clients
  must treat the cursor as an opaque string (the contract already says
  "decimal cursor"; the README now names both forms).
- **This is a wire-format change for supervisor items** (`"1"` becomes `"1.1"`)
  and reverts `maxItems` to 16. That needs the matching dashboard consumer
  change (`CROSS-REPO FOLLOW-UP (aq-dashboard)` task comment).
- **`REPEATABLE READ, READ ONLY`, not autocommit reads.** The head and
  generation rows are still read from one snapshot, so `inspect` never pairs
  a head with a generation from a different commit.

## Final merge

`git merge origin/main` (merge `5ec52007`) brought in 87.2.1's reservation
consumption (migration 0049) and supervisor-host credential and adapter work.
It merged without conflicts. The logs in `F1b-cursoring-evidence/` are from
the post-merge reruns:

- passing: `acceptance` (1 passed; workspace `--exact`), `inspect` (10),
  `conversation` (6), `api` (2) and `clippy` (clean);
- `host`: 11 passed, 1 failed. The failure is
  `main_delivery_database_upgrades_without_losing_history`
  (`supervisor_session.rs:618`): left `"49"`, right `"48"`. It already fails
  on `origin/main`, which added migration 0049 without updating that literal.
  It is not caused by this change; before the merge `host` was 12/12. It is
  recorded as a task finding.

The `central37` and `format` logs are from before the merge (7 passed;
clean).

## Reopen verification (claim epoch 2)

The operator merged `origin/main` (`7c2a32f5`, then `7d71eaf6` at
`0376a910`). Reruns on that head: `inspect` 10 passed, `api` 2 passed,
`conversation` 6 passed, `host` 11 passed and 1 failed. The failure is still the
pre-existing `main_delivery_database_upgrades_without_losing_history` literal
(left `"51"`, right `"48"`; `origin/main` now ships migrations 0049-0051).
`owner_message_reaches_supervisor_and_reply_is_recorded` passed inside `host`.
