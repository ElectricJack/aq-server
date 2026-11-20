# 0003 — Live state only: implementation notes

Spec: `docs/specs/0003-live-state-only.md`. Plan:
`docs/superpowers/plans/2026-11-13-live-state-only.md`. Engine crates
`aq-playbooks-runner`, `aq-playbooks-postgres` and `aq-playbooks-cli` are
**0.2.0** (ruling SC-17; the workspace pins are `=0.2.0`). Model and
compiler are unchanged at 0.1.0.

The record of a namespace now holds only work in progress and a rolling
window of remembered keys. A finished cohort leaves it in the commit that
finishes it, as history rows; nothing finished is ever counted against new
work, and there are no namespace limits. Acceptance results are in
"Acceptance results" near the end; the per-verdict JSON is in
`0003-live-state-only.results.json`.

Contents, in order: what changed; the spec-check rulings; request time and
stale requests; a pinned retention clock; no namespace limits; the rolling
repeat protection; the rolling history; the drain of older namespaces;
`collect_garbage`; per-commit bounds; the lost-handle hold; PostgreSQL
notes; the history table; format 3; acceptance; known boundaries; the list
of rewritten tests.

## What changed

File:line at the 0.2.0 commit.

**Runner (`crates/aq-playbooks-runner/src`)**

- `lib.rs:626` `State::settled` (never stored): the cohorts a commit
  removes, handed to storage as history in the same commit. `history.rs:31`
  `SettledCohort`, `history.rs:39` `history_items` (one row per run, effect
  and receipt; a run row carries its terminal receipt's sequence).
- `lib.rs:840` `transaction_report_at`: every commit with a host time ends
  in `retention::finish_commit` (`retention.rs:2034`): settle the cohorts
  the commit finished (`settle`, `retention.rs:986`; `remove_cohorts`,
  `retention.rs:1069`), one bounded drain step for older namespaces
  (`drain_step`, `retention.rs:2112`), the window drop (`drop_expired`,
  `retention.rs:1549`), the lost-handle lapse and re-judge (`lapse_holds`
  `retention.rs:1668`, `rejoin` `retention.rs:1716`) and the eviction past
  `replay_keep` (`evict`, `retention.rs:1819`). A licensed admin commit ends
  in `finish_licensed_commit` (`retention.rs:1331`) instead.
- `retention.rs:23` `RetentionPolicy`: `admission_replay_ms`,
  `replay_keep`, `history_keep` and every per-commit bound
  (`window_drops_per_commit`, `replay_evictions_per_commit`,
  `history_trim_per_commit`, `gc_page_limit`, `settle_cohorts_per_commit`,
  `drain_cohorts_per_commit`, `drain_entries_per_commit`), all
  serde-defaulted. `retention.rs:157` `RetentionState` gains the clock,
  expiry order, eviction order, holds, re-judge queue, stale floor and
  `drain` (`DrainState`, `retention.rs:213`).
- `retention.rs:730` `verify_removals`: the removal fences every backend
  runs (nothing leaves without history or expiry; the eviction licence
  `check_evictions`, `retention.rs:1947`; the admin licences).
- `lib.rs:362` `refuse_stale` and `lib.rs:46` `Error::Stale`;
  `admission.rs:98` `Engine::admit_once_at`, `lib.rs:1087`
  `Engine::admit_event_at` (request time).
- `lib.rs:3800` `rebase_retention_clock`, `lib.rs:3824` its dry run,
  `lib.rs:3841` `lower_stale_floor`.
- `lib.rs:3879` `collect_garbage`: one ordinary commit plus the artifact
  phase (`retention::gc`).
- `lib.rs:754`, `lib.rs:758`: one-time backfills on open (`root_intents`
  for pre-D10 namespaces; operations' first dispatch sequence).
- `lib.rs:795` `Engine::inspect` answers from the live State and history;
  `lib.rs:803` `delete_history_before`.
- `storage.rs:55`/`59`/`68`: the `Storage` trait's `persists_history`,
  `history_page`, `delete_history`; `Engine::new` refuses a backend without
  history. `storage.rs:134` `MemoryStorage::retaining_settled` (tests).
  `transition.rs:757` the same for `RecordStorage`.
- The quota mechanism is removed (see "No namespace limits").

**PostgreSQL (`crates/aq-playbooks-postgres`)**

- `migrations/0006_run_history.sql`, `migrations/0007_run_history_trim.sql`
  (see "History table").
- `src/history.rs:66` `insert` (with the trim, `history.rs:86`),
  `history.rs:187` `page`, `history.rs:222` `delete_before`; the protected
  and plain format 2 commit writes history at `src/lib.rs:795`, format 3 at
  `src/records.rs:480` and `:586`.
- `src/lib.rs:451` `inspect`: live rows and history, each branch bounded.
- `src/retention.rs:44` `drain_tombstones`: legacy `disposal_tombstones`
  rows, a bounded number per commit.
- `src/lib.rs:129`/`142`: `ANALYZE` at open for engine tables without
  statistics.
- `src/indexes.rs`: receipt index rows keyed by sequence.

**CLI (`crates/aq-playbooks-cli/src`)**: run, cancel and simulation views
read live state plus history (`simulation.rs:153`, `main.rs`);
`rebase-retention-clock` and `lower-stale-floor` subcommands; `gc --help`
says it is not needed.

**Benchmark** (`crates/aq-playbooks-postgres/examples`): `engine_growth`
gains the `steady` and `burst` acceptance modes, the per-table on-disk
sizes, and the capture of every removed run's projection;
`engine_growth_drain` gains `upgrade` (acceptance 4).

## Spec-check rulings applied

The plan's spec check (`docs/superpowers/plans/2026-11-13-live-state-only.md`,
"Spec check") and the spec's "Rulings after the builder check":

| Ruling | Applied as |
| --- | --- |
| NR-1 | No correlation fences. A wait registered after its event's window times out; it is not refused at registration. |
| NR-2 | Acceptance 2's last clause: 20 runs at a frozen clock past the window drain the burst; the clock moves past the window once more; one more run; compare with run 1. The literal figure is reported too. |
| SC-1 | Request time is an optional argument of `admit_once_at` / `admit_event_at`, in no digest. The cut-off millisecond is stale (corrected in Task 7). |
| SC-2 | Removal fences moved into `retention::verify_removals`, used by every backend. |
| SC-3 | A cancelled operation is settled; its cohort leaves at once and a late result gets `NotFound`. |
| SC-4 | History is its own insert-only table, one row per inspection item. |
| SC-5 | Expiry order: one record per entry (`window_key`), oldest first. |
| SC-6 | Settlement found from the commit's dirty roots, O(work of the commit). |
| SC-7 | Event destination keys include the acceptance sequence, so a re-accepted event's run ids do not collide. |
| SC-8 | `admin` and `event:<key>` receipts go to history (rolling sets of their own). |
| SC-9 | An expired event that live work needs is deferred by its own window (Task 6 fix: by the real window). |
| SC-10 | An expired keyed receipt whose run is live is held. |
| SC-11 | Commits without host time move no clock and drop nothing; the clock never goes back. |
| SC-12 | History rides on `State::settled`; three new `Storage` methods; `Engine::new` refuses a backend without history. |
| SC-13 | Format 3: cohort removal stays a heavy commit (see "Format 3"). |
| SC-14 | `collect_garbage` is an ordinary commit plus the artifact phase. |
| SC-15 | Legacy `disposal_tombstones` rows: at most `drain_entries_per_commit` deleted per commit. |
| SC-16 | The 2 KB burst bound stands; measured below. |
| SC-17 | Runner, postgres and CLI are 0.2.0. |

Later owner direction (spec 0003, "rolling bounds" and "no limits")
replaced §3–§4's quotas: `replay_keep` and `history_keep` are rolling sets,
and there are no namespace limits.

## Request time and stale requests

After its window an event or keyed admission receipt leaves the record, and
the engine no longer remembers its key. A request that repeats such a key is
judged by time, not by a remembered key.

### Which requests carry a request time

| Engine method | Request time | Window |
| --- | --- | --- |
| `Engine::admit_event_at(producer, event, epoch, requested_at_ms, now_ms)` | `requested_at_ms: Option<i64>` | `max(policy.event_replay_ms, contract.retention_ms)` |
| `Engine::admit_once_at(request, requested_at_ms, now_ms)` | `requested_at_ms: Option<i64>` | `policy.admission_replay_ms` (default 7 days) |
| `Engine::admit_event`, `Engine::admit_event_in_epoch` | none (they pass `None`) | — |
| `Engine::admit_once` | none (it passes `None`) | — |

No other request type carries one. The CLI has no event or keyed-admission
command. In aq-playbook-host, `Request::Event` and `Request::AdmitOnce` are
the carriers. Until the server switches to the `_at` methods (aq-server
follow-on), every request goes through with no request time.

The request time is the producer's time for the *request*: for aq-server,
the time it first created the admission or the outbox event. It is not the
event's business time (`occurred_at_ms` / `observed_at_ms`). A calendar
trigger sets that to the due date, which can be days old. The request time is
not part of the event digest, the keyed request digest or any stored record.
It never moves the retention clock.

### The rule

1. **A key the engine still remembers answers as before, whatever its
   request time.** Inside its window a repeat is a duplicate, and changed
   content is a `Conflict`. The same holds after the window while the
   entry is held: a keyed receipt whose run is still live, or an expired
   event that a live wait, a queued destination or an unknown completion
   handle still needs.
2. **Otherwise** (the key is absent, or expired and held by nothing), a
   request with `requested_at_ms <= max(now, clock) − window` is refused
   with `Error::Stale`, and nothing is written.
   - The cut-off millisecond itself is stale. This corrects plan SC-1,
     which accepted it. An entry accepted at window start `S` expires at
     `S + w`, and from that millisecond on the engine no longer remembers
     it. A retry carrying its original request time `R ≤ S` then has
     `R ≤ start − w`, so "expired" and "stale" are exact complements: an
     honest retry can never be admitted a second time.
   - When a clock rebase recorded a stale floor (below), the cut-off is the
     later of that and the floor.
   - For an expired, unheld entry still in the record, the check runs
     *before* the commit that forgets it. A stale request leaves the entry
     and the clock where they were.
3. **No request time** (`None`) is not checked. An expired key is then
   accepted as new. This is the cost of keeping nothing. A host that needs the
   refusal sends the request time.

`Error::Stale { requested_at_ms, cutoff_ms, clock_ms, now_ms, window_ms,
floor_ms }` is a variant of its own. Hosts match the variant, not the text.
Its `Display` starts with `STALE_REQUEST` (`"stale request: producer time is
older than the replay window"`) and names the request time, the cut-off, the
retention time, the window and the host time (and the floor, if any).

**For the server follow-on.** aq-server's event loop fails the whole quantum
on any engine error except `Overloaded` (`driver.rs:752`, `delivery?`). When
it adopts the `_at` methods, it must treat `Error::Stale` as an outcome of
that one request: retire or hold that outbox row or admission. It must not
fail the quantum, or one stale row would stop every event behind it.
aq-playbook-host's `runner_error` turns every engine error it does not know
into a string (`other => Error::Engine(other.to_string())`), so it needs an
`Error::Stale` arm of its own.

### What a request time must be

The engine trusts the host for the request time, as it does for the key.
For the rule to hold, the host must:

- take it from the same clock as the host time it passes as `now_ms`;
- stamp it once, when the request is created (the outbox row or the
  retained admission), and never re-stamp it on a retry;
- never let it be later than the host time of the request's first
  submission. If the request time is `d` ms ahead, a retry within `d` ms
  after the expiry is admitted as new.

**A request time in the future is accepted** (it is never stale). It is
compared and nothing more: it does not move the clock, does not change any
expiry, and cannot make a remembered key new. It behaves exactly like
`None`. Refusing it would add no protection against a trusted host, since
re-stamping to "now" defeats the past-time check just as well. It would also
make a second outage: a row stamped during a forward clock jump that no
commit carried would be refused until real time caught up.

### Known gaps

- **Raising a window.** The stale window is the *current* policy or
  contract window, while each entry's expiry was fixed with the window that
  applied when it was accepted. After a window is raised, a key accepted
  under the old, shorter window and already forgotten can be retried with
  its original request time and admitted as new, as long as that time is
  within the new window. The gap lasts one new window after the raise.
- **Two-commit race.** Re-admitting an expired, unheld entry takes two
  commits. The stale check runs in both, against each commit's clock. If a
  commit with a later host time lands between them, the second may refuse
  as stale after the first has already forgotten the expired entry. Nothing
  protected is lost: the entry had expired and nothing held it. Only
  concurrent use of one `Engine` can hit this. aq-playbook-host serializes
  on an owner thread.

## A pinned retention clock

Staleness is judged against `max(now, clock)`, the same time expiry is
judged against. The retention clock is the maximum host time any commit has
carried, so one far-future host time `P` pins it. From that commit on:

- **Entries are forgotten early.** Every window entry has expired at `P`
  and drains at 64 per commit, up to a full window early. Retries of those
  keys without a request time (today's callers) are admitted as new.
- **Every request with a real request time is stale**, because the cut-off
  is `P` minus the window. With `_at` callers, event ingestion and keyed
  admission stop. Requests without a request time still go through.
- **The record grows.** What those requests write expires from the pin (an
  event at `P + w`, a keyed receipt at `P + admission_replay_ms`), and so do
  deferrals. `events`, `keyed_admissions` and the expiry order grow with
  traffic until real time reaches `P`. On protected format 2 every commit
  re-encrypts the whole record, so commit cost and write volume grow with
  it. In the end `max_events`, `max_receipts` or `max_state_bytes` refuse
  every admission (`Overloaded` in aq-playbook-host), including those
  without a request time.
- Activation receipts are dated `P`.
- What keeps working: driving existing runs, plain `admit`, inspection,
  history deletion, and duplicates of remembered keys.

### Recognising it

The stale refusal says when the clock is ahead of host time. Example (window
600,000 ms):

```
stale request: producer time is older than the replay window: request time
1700000000000 ms is at or before the cut-off 8999999400000 ms (retention time
9000000000000 ms minus the 600000 ms window; host time 1700000000000 ms); the
retention clock (9000000000000 ms) is ahead of host time by 7300000000000 ms,
and while it is, every request that carries a real request time is refused as
stale (a clock pinned by a far-future host time stays ahead until it is
rebased)
```

The signs are a "retention clock (…) is ahead of host time by …" part that is
much larger than normal clock skew, and that it appears on every request.
`Error::Stale.clock_ms` and `now_ms` carry the two times. The State's
`retention.clock_ms` shows the pinned value.

## If the clock was pinned

Three admin operations, each one licensed commit with its own admin receipt
in history:

- `Engine::rebase_retention_clock_dry_run(to_ms, stale_floor_ms, actor,
  reason, now_ms)` makes the same checks and returns the same report, on a
  snapshot. It commits nothing.
- `Engine::rebase_retention_clock(to_ms, stale_floor_ms, actor, reason,
  now_ms)` is the rebase.
- `Engine::lower_stale_floor(to_ms, actor, reason, now_ms)` undoes a floor
  set too high (see "The stale floor" below).

From the CLI:

```
# 1. Preview: old and new clock, floor, and what would be capped.
aq-playbooks rebase-retention-clock --dry-run --to-ms <corrected real time> \
  --stale-floor-ms <last correct host time before the pin> \
  --actor <who> --reason <why> --database-url … --namespace …
# 2. Commit.
aq-playbooks rebase-retention-clock --confirm-clock-rebase --to-ms … \
  --stale-floor-ms … --actor … --reason … --database-url … --namespace …
```

The CLI passes the machine's own time as `now_ms`.

### Preconditions

A mistaken rebase must not be able to do lasting damage. The rebase is
refused unless all of these hold:

- **The clock is pinned.** It is ahead of `now_ms` by more than the smallest
  policy window (`min(admission_replay_ms, event_replay_ms)`). That is the
  only case in which the pin refuses anything. A clock ahead by less cannot
  be rebased.
- **The target is the corrected real time.** `now_ms <= to_ms <= now_ms +
  slack`, where the slack is one minute, or a tenth of the smallest window
  if that is less. Choose `--to-ms` slightly ahead of every host's clock,
  within the slack. A target below a correct host's time would cap entries
  that host accepted at `A > to_ms` below `A + window`. A target much later
  than real time would itself pin the clock.
- **The floor is in the past:** `stale_floor_ms <= now_ms`. It is the last
  correct clock before the pin.
- An actor and a reason, both present and bounded.

**`now_ms` must be right.** Like every commit, the rebase trusts the host
time it is given, and the fence cannot know real time. A rebase with a wrong
`now_ms` (say 0) is accepted if the clock is far enough ahead of it. It then
moves the clock to that time and caps every window from there, far behind
real time. The engine checks only what it can see without scanning the
record: the raised floor must still be at most the target, so a host time
earlier than an earlier rebase's floor (less the slack) is refused. The rest
is the host's job:

- the server takes `now_ms` from its own clock and cross-checks it, and
  `to_ms`, against PostgreSQL `now()` (see "When aq-server exposes these
  operations");
- the machine running the CLI must have an NTP-synced clock;
- run the dry run first and read the old clock, the new clock and the
  counts.

### A clock ahead by less than the smallest window

That clock cannot be rebased, and it does not need to be. No honest request
is refused as stale, because the cut-off stays behind real time. Entries
expire at most that gap late. Wait it out: as real time passes the bad host
time, the clock is correct again.

Timers registered by commits at the bad host time (waits, joins, retries,
operation deadlines, run durations) fire up to that gap late, which is less
than the smallest window (up to 1 day with the default event window). Cancel
(`aq-playbooks cancel <run>`) or reconcile any run whose late timer matters.

### What the rebase does

1. It sets the retention clock to `to_ms`.
2. It caps every recorded expiry later than its own cap at exactly that
   cap. The cap is `to_ms` plus the admission window for a keyed receipt,
   and `to_ms + max(event_replay_ms, contract.retention_ms)` for an event,
   never the bare policy window when the contract window is longer. An
   expiry at or below its cap stays.
   - What the pin wrote expires in real time again, and no entry is
     remembered for less than its own window from `to_ms`.
   - Held entries stay, and are deferred again in the normal way.
   - Entries the pin had not yet drained become unexpired. They are
     remembered longer, which is harmless.
3. It caps every live run's timer that lies beyond `to_ms` plus its
   declared duration at that value:
   - a wait (`Wait.deadline_ms` of its step);
   - a join (`Join.deadline_ms`);
   - a retry (`RetryPolicy.backoff_ms`);
   - an operation deadline (`command.deadline_ms`, stored with the
     operation);
   - the run's duration basis: `admitted_at_ms` is lowered to `to_ms`, so
     `limits.max_duration_ms` counts from there.

   Each capped value is still at least its real registration plus its
   duration, because the registration was at or before `to_ms`. A wait set
   at the pinned time then times out normally, and the run settles and
   leaves the record. Every kind's duration is in the artifact or the
   operation, so a consistent State never needs a guess. A deadline after
   `to_ms` whose duration cannot be found (an inconsistent State, such as a
   retry on a step without a retry policy) is left as it is. Its run is
   listed in `needs_operator_action`: cancel it (`aq-playbooks cancel
   <run>`), or reconcile its operation.
4. It raises the stale floor to the later of the existing floor and
   `stale_floor_ms`.
5. It writes a `retention_clock_rebased` admin receipt. The receipt holds
   `from_ms`, `to_ms`, `stale_floor_ms`, the capped counts, `adjusted_runs`,
   `needs_operator_action`, `actor`, `reason`, and the host time as `at_ms`.
   For audit it also lists each capped timer (`adjustments`: run, kind,
   item, original and capped value), at most 1,000 of them; the rest are
   counted in `adjustments_not_listed`. That is how a run's history row,
   which shows `admitted_at_ms = to_ms` next to receipts dated at the bad
   time, can be traced back. The report returns the same.

The commit does nothing else. It carries no host time, so nothing settles
and nothing is dropped in it. Its cost is one pass over the expiry order
above the smallest cap, plus one pass over the live runs and operations.  It
is at most one record's worth (the remembered window is at most
`replay_keep` unheld entries plus those live work holds), the same order as
any protected format 2 commit.

### The fences

Without a licence, the Task 4 fences are unchanged: the clock never
decreases, a recorded expiry only moves later, and the stale floor never
changes, not even upwards.

A commit that carries a licence receipt (`retention_clock_rebased` or
`stale_floor_lowered`) in its own history must be exactly that licence.
Otherwise it is refused: `REFUSE_LICENCE`, or `REFUSE_EXPIRY` for a wrong cap and `REFUSE_STALE_FLOOR` for a wrong floor. This is checked by
`retention::verify_removals`, used by the in-memory store, the record
store's full commit, and PostgreSQL protected and plain format 2 and format
3. The record store's patch path checks the same rules. A format 3
transition that carries a licence takes the complete path.

- **The receipt is new.** Its sequence is after the stored sequence. A
  receipt cannot be replayed, even after `delete_history_before` removed its
  history row. At most one licence per commit.
- **A rebase** lowers the clock from exactly the receipt's `from_ms` (the
  stored clock) to exactly its `to_ms`, with `to_ms < from_ms`. It may lower
  a recorded expiry only to exactly its cap (above). It may only raise the
  floor, to at most `to_ms`.
- **A floor lowering** leaves the clock alone. It lowers the floor from
  exactly the receipt's `from_ms` (the stored floor) to exactly its `to_ms`,
  and changes no expiry.
- **Neither** adds or removes a run, operation, event or keyed receipt, or
  removes an audit receipt.
- **Neither** changes any header field its licence does not name: the
  retention policy, producer epochs, GC state, artifact disposals,
  artifacts, bindings, the catalog revision and history, execution denials,
  authority revocations or recovery requests.
- **Caps are computed from the stored state.** The policy, the installed
  contracts and the stored event are those before the commit, so a licensed
  commit cannot shrink a cap by changing the policy, a contract, or an
  event's contract reference.

So a receipt for another clock or floor, a receipt left in the live State,
a replayed receipt, or a licensed commit that also does other work is
refused. Run timers are not fenced, by any commit, as before.

### The stale floor

**What it guarantees.** After a rebase, a request for a key the engine does
not remember whose request time is at or before the floor is stale. The
floor never changes what happens to a key the engine still remembers. Once
`max(now, clock)` reaches `floor + window`, the ordinary cut-off is later
than the floor anyway. The floor must be in the past, so its extra effect
lasts at most one window.

- If the floor is at or after the last correct clock before the pin, every
  key the pin forgot early is covered. Their honest retries carry the
  request time of the original request, which was at or before that clock,
  so they are refused as stale, not admitted a second time.
- It cannot protect:
  - **retries with no request time.** A key forgotten during the pin and
    retried without a request time is accepted as new, as the spec already
    accepts for any expired key;
  - **keys whose request time is after the floor.** If the floor is set
    earlier than the pin started, keys forgotten early with request times
    between the floor and the pin can still be admitted again.
- **It also refuses new requests created at or before it** that the engine
  never admitted. Requests stamped after the floor that the pin refused
  are accepted after the rebase. Set the floor as late as covers the pin,
  and no later.
- Finding the last correct clock: the host's logs, or the newest history
  item or receipt dated before the jump (the first item dated far in the
  future is the jump).

**A floor set too high** (by a mistaken rebase) can be lowered:
`aq-playbooks lower-stale-floor --to-ms <value> --actor … --reason …
--confirm-floor-lowering`, or `Engine::lower_stale_floor`. It accepts that
keys forgotten after that time may be re-admitted as new by a retry with a
request time in between. The floor goes down only to the value the operator
states, in a commit of its own with a `stale_floor_lowered` receipt.

### When aq-server exposes these operations

The engine does no authority check. `actor` and `reason` are free text. The
server must:

- allow the operations only to an operator or admin role;
- set `actor` from the authenticated identity, not from the request;
- pass its own clock as `now_ms`;
- check `to_ms` (and the floor) against PostgreSQL `now()` before calling,
  and refuse when they disagree beyond the slack;
- require explicit confirmation, and offer the dry run first.

## No namespace limits

Owner direction (spec 0003, final section): there are no namespace limits.
The record holds work in progress. Finished work is deleted when it
finishes. Remembered keys and history are rolling sets of fixed size
(`replay_keep` and `history_keep`).

Nothing is ever refused because a namespace holds too much: no admission
(direct, keyed, or an event's fan-out), event, step, child start,
completion, wait, timer, cancel, settlement or admin commit. What bounds the
record is deletion alone.

Removed in Task 8b (the mechanism Task 8 built, and everything that served
only it):

- the quota check (`check_quota`, `over_quota`, `quota_counts`,
  `QuotaCounts`, `QuotaGate`, `quota_ceiling`), the `limit + limit / 8`
  ceiling, the commit classes (`QuotaMarks`, `State::quota_marks`) and the
  quota refusal texts;
- `TransitionRequest::validate` (the format 3 path's quota callback);
- the maintained byte and queued-destination sums of the format 3
  accounting (`Accounting` keeps each container's record count, which a
  bound container's `len` reads, and the header length). A PostgreSQL
  format 3 row written before stores `[count, bytes]` per container and a
  queued-destination count; both decode, and the extra numbers are ignored;
- the `with_config` refusal of zero or small limits
  ("invalid namespace resource bounds").

**Deprecated, accepted and ignored**, so stored states decode and existing
hosts compile:

- `EngineConfig::{max_runs, max_events, max_queued_destinations,
  max_state_bytes, max_receipts}`. `EngineConfig` decodes with any of them
  missing (`#[serde(default)]`), and `Engine::with_config` keeps the value
  unread;
- `RetentionPolicy::{max_tombstones, terminal_run_ms}`. A stored policy
  decodes with or without them (each has a serde default) and behaves the
  same either way (tested: `deprecated_limit_fields_decode_and_change_nothing`).

`set_retention_policy` refuses only a zero window (`event_replay_ms`,
`admission_replay_ms` or `audit_ms`): "retention windows must be positive",
and a zero per-commit bound (see "Per-commit bounds are policy fields").
PostgreSQL does not count `disposal_tombstones` rows on a full commit; the
GC clock's cutoff delete (`collect`) takes artifact rows only. Rows of
runs, operations and events written before this change are deleted by the
drain, a bounded number per commit, and never refuse a commit.

**What stays, because it is not a namespace limit:** the limits a playbook
declares for itself in its artifact (step visits, children, depth,
iterations, duration, concurrent effects and children, payload and history
bytes), which bound one run and are part of the playbook's meaning; input
validation bounds (payload sizes, key and text lengths); the bounded amount
of deletion work per commit (window drops, evictions, drained cohorts and
entries, the history trim batch, all policy fields); and the removal fences, which are about the
correctness of deletion, not capacity.

**A namespace written before this change** still holds the runs that
finished before the upgrade until the drain moves them out (Task 9, see
"Draining a namespace an older engine wrote"). They never make new work
wait (tested:
`a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains`).

## Repeat protection is a rolling set (`replay_keep`)

Owner direction (spec 0003, "rolling bounds"). The engine remembers the most
recent `policy.replay_keep` window entries (default 256; events and keyed
admission receipts counted together) that no live work holds, and forgets an
entry when its window passes *or* when newer entries push it out, whichever
comes first.

**What it guarantees.** A repeat is recognised (answered as a duplicate, or
`Conflict` for changed content) while its window has not passed *and* fewer
than `replay_keep` newer unheld keys have been accepted since. An entry that
live work holds is never evicted, at any count:

- a keyed admission receipt whose run is in the record;
- an event with a live waiter on one of its correlation keys, a queued
  destination its input does not block, or an uncertain completion of its
  contract and scope (the same holds as the window's deferral, SC-9).

**What it does not guarantee.** A repeat that arrives after `replay_keep`
newer unheld keys is not recognised. It is treated exactly as a repeat of an
expired key (Task 7's rule): refused as stale only if it carries a request
time older than its window; otherwise accepted as new, so the work runs
again. There is no "eviction floor": the owner chose bounded memory over
catching every slow duplicate, and a floor would refuse legitimately new
work whose request time is old.

**It overrides a contract's `retention_ms` for unheld events.** Every event
contract in the AQ host catalog declares 24 h retention, so the time window
alone kept a day of events in the record. Count eviction removes an unheld
event long before that. A wait registered after its event was evicted
times out, as after an expired event (tested:
`a_day_long_event_window_is_evicted_by_count`).

**A host that needs stronger protection raises `replay_keep`** (the record
then holds up to that many unheld entries, about 1 to 2 KB each), or sends a
request time so a late repeat is refused as stale.

**Measured** (Task 8b, `engine_growth` config `a`, protected format 2,
10-minute windows, default `replay_keep` 256): with the clock frozen for
1,000 runs (one event and one keyed admission each), the record grows to
278.0 KB by run 200 and stays there (278.3 KB at run 500, 278.4 KB at run
1,000), holding 128 events and 128 keyed receipts; the median step is
25.6 ms at run 1,000. Before this change the same burst grew 2,014 B per
run without end. At 10 s per run the 10-minute window, not the count,
bounds the record (about 120 entries; 132.7 KB at run 200).

### How it works

- `RetentionState::replay_order` holds the unheld entries in acceptance
  order, one record each, keyed `replay_key(sequence, member)`: 16 hex
  digits of the acceptance sequence (an event's `sequence`; for a keyed
  receipt, the sequence in its run id `run-<16 hex>`), then the member
  (`e:<event key>` or `k:<admission key>`).
- A keyed receipt is held by its live run from its acceptance:
  `RetentionState::replay_held` maps the run id to the key, and the receipt
  joins the order in the commit its run leaves the record (in its history).
- An event is judged (`event_held`) in the commits that may change its hold:
  the one that accepts it, the one that fans it out, and any commit that
  changes the waiters of one of its correlation keys (`State::replay_touched`,
  never stored, filled by the index sync and by cohort removal). Held, it is
  out of the order; unheld, in it.
- A hold through an uncertain completion: an asynchronous operation
  without a handle (dispatched, or with its admission handle lost, or
  prepared again by recovery after such a dispatch) may be completed by an
  event of its contract and scope, but only one accepted after its first
  dispatch. The engine records the first dispatch's sequence on the
  operation (`Intent::dispatch_sequence`; a re-dispatch keeps it, since an
  idempotent re-delivery's completion may answer the first), and such an
  operation holds only events accepted after it. An operation never
  dispatched (stopped by revoked authority before dispatch) is not
  uncertain and holds nothing. When a contract's uncertain operations
  change, the commit judges again only the events of that contract
  accepted after the earliest dispatch concerned
  (`indexes.contract_events`, ordered by sequence). So a dispatch judges
  no event at all, the binding of its handle judges the events that
  arrived during the host call, and while a handle is lost its later
  events are held (never evicted, not counted) and rejoin at their
  acceptance positions when it is found. Operations an older engine dispatched
  get their dispatch sequence once on open, from their own first
  `dispatch` receipt (see the drain, below). A State written before the
  index existed fills it through the drain.
- `retention::evict`, in every commit that changes the State except a
  licensed admin commit, after the window drop: while the order holds more
  than `replay_keep`, take its head, at most
  `policy.replay_evictions_per_commit` (default 64) per commit (counted
  apart from the window drop's own bound). A held head
  leaves the order; otherwise the entry is forgotten exactly as when its
  window passes (item, expiry record, expiry-order key; for an event its
  epoch, indexes and `event:<key>` receipts, which go to history). It never
  refuses, never delays the commit and needs no host time. A commit with
  nothing to evict compares one count; k evictions cost O(k) (tested on a
  plain State and on bound records).
- A commit lowers `replay_keep` (`set_retention_policy`); the excess leaves 64
  per commit. `0` is refused ("replay_keep must be positive").

### The eviction licence (the removal fences)

`retention::check_evictions`, run by `verify_removals` on every complete
path (memory and record CAS, PostgreSQL protected and plain format 2,
format 3 CAS and heavy), by the record store's patch fence, and by the
PostgreSQL format 3 bounded path while its transaction is still lent (so an
eviction stays an incremental commit). A keyed receipt or event removed
before its expiry, whose order key `next` no longer holds, is allowed only
as `evict` makes it:

- no live work in `next` holds it (a keyed receipt's run is not in `next`;
  an event is judged from the live records: a run of `next` still waiting
  on a wait record whose correlation key is one of the event's, an
  unsettled operation waiting for its completion, an uncertain operation of
  its contract, a queued destination of the stored event; the candidate
  waiters and uncertain operations are those of `next`'s index and of the
  stored one, so a commit cannot release a hold by rewriting the index. Cost
  per evicted event: its correlation keys × their waiters, one run and one
  wait record each; at most 64 evicted events per commit);
- at most 64 in the commit, and `next`'s order still holds at least
  `replay_keep` entries (the order held more before them);
- oldest first: each evicted key is before every key `next`'s order holds.

An accepted event's identity (key, sequence, digest, acceptance time,
envelope) never changes: on every path a commit that rewrites it is refused
with "immutable accepted event changed" (its destinations may change:
fan-out). So an event cannot be moved to the head of the order.

And every order key a commit adds must be the key of an item `next` holds,
at that item's own acceptance sequence, that no live work holds, so the
count that licenses evictions counts real unheld entries. A violation is
refused with "window entry evicted without its licence" (an entry the
stored order did not hold: "window entry removed before its expiry"). An
entry that joins the order and is evicted in the same commit (a keyed
receipt whose run has just left, the oldest) is allowed. A removal whose
order key stays is no eviction and is refused as early.

### Stored shape

Additive and defaulted: `RetentionPolicy::replay_keep` (256) and
`history_keep` (10,000), `RetentionState::replay_order` and `replay_held`
(two more lazy containers on format 3, `retention.replay_order` and
`retention.replay_held`). States written before decode with the defaults.
A namespace's snapshot bytes change on its next commit even when nothing is
evicted: the policy serializes its two new fields, the two maps their
entries (about 90 bytes per remembered entry in `replay_order`, plus the
`replay_held` entry of each live keyed run).

**Entries written before this change** join the order through the drain
(Task 9, below): each keyed receipt and event gets its expiry record,
window key and place in the order, a bounded number per commit.

## History is a rolling log (`history_keep`)

History is three rolling sets of `policy.history_keep` (default 10,000)
each, per namespace:

- finished runs, each with its effects and receipts;
- the receipts of events (`event:<key>`: delivery receipts such as
  `destination_blocked`), an event's receipts leaving together;
- `admin` receipts (operator actions). They take `history_keep` too: a
  separate constant would add a setting without a use, and operator actions
  are far rarer than runs.

Nothing in history is outside a set, so history holds at most
`history_keep` finished runs (each with the items its playbook's own limits
allow), `history_keep` event receipts and `history_keep` admin receipts.

Each commit that writes history deletes the oldest of each set beyond
`history_keep`: at most what it writes of that set plus
`policy.history_trim_per_commit` (default 64), so it keeps up with any commit and a
lowered `history_keep` is reached within a bounded number of commits. A run
is deleted with all its items first and its `runs` row last, in the same
transaction. The bound counts units, not rows: 64 runs of 1,500 receipts
each are about 100,000 row deletions in one commit (each index-backed).
`history_keep` 0 writes no history at all; inspection of finished runs
then returns nothing, and what history there was is trimmed by the next
commits that settle a cohort.

**"Oldest" is by the namespace's sequence**, never by host time: a run row
carries its run's terminal receipt's sequence (else its cohort's last
receipt's), a receipt its own. A host clock that ran ahead therefore cannot
make the newest runs the first to go (tested:
`memory_storage_trims_by_sequence_not_finish_time` and the PostgreSQL
`history_trims_by_sequence_and_bounds_receipts_of_no_run_on_postgres`).
History rows written before this change carry sequence 0 for runs and are
trimmed first.

Inspection's page order, cursors and filters are unchanged. A page whose
cursor's run is trimmed before the next page neither repeats nor skips a
remaining item: the next page starts after the cursor (tested:
`a_page_across_a_trim_neither_repeats_nor_panics`).

`Engine::delete_history_before(before_ms)` (host-called, by finish time)
keeps working and keeps the counts right, so the trim after it is still
exact. It is optional: history is already a rolling log. It is an `Engine`
method, so any host holding the engine reaches it (on PostgreSQL it needs
the engine's lease); it commits no State and moves no clock. aq-server's
owner thread (`aq-playbook-host`) has no request for it yet: a server
follow-on, if the server wants to delete history by time.

### PostgreSQL (migration 7)

- Three partial indexes on `run_history(namespace, sequence, item_key)`:
  `run_history_runs_by_sequence` (`kind='runs'`),
  `run_history_event_receipts_by_sequence` (`kind='receipts' AND run_id LIKE
  'event:%'`) and `run_history_admin_receipts_by_sequence` (`kind='receipts'
  AND run_id='admin'`). The trim reads the oldest of each set from them in
  order: no sort, no scan of the table.
- `run_history_counts(namespace, runs, event_receipts, admin_receipts)`:
  what each namespace's history holds of each set, maintained by the
  transactions that insert, trim and delete history, so nothing ever counts
  rows. The migration fills it once from existing history. Writes need the
  namespace writer, like `run_history`.
- The trim: read the counts; take the oldest `min(count - keep, written +
  64)` of each set through its index; delete a run's items through
  `run_history_by_run` then its row through the primary key, an event's
  receipts through `run_history_by_run`, admin receipts through the primary
  key; lower the counts (tested with `EXPLAIN` over 120,000 rows:
  `history_trim_reads_and_deletes_through_indexes`).
- **Privilege.** The migration runner applies pending migrations in one
  transaction as the connecting role. Migration 7 creates indexes on
  `run_history` (which needs ownership of that table) and creates a table
  in schema `aq_playbooks` with a foreign key to `namespaces` and triggers
  on itself. That is exactly what migration 6 needs of the role that
  applies it (it creates `run_history`), so the role that applied
  migration 6 can apply migration 7, and nothing more is needed. Checked
  by hand on PostgreSQL 17 with a `NOSUPERUSER NOCREATEDB NOCREATEROLE` role
  owning the schema: 1-6 then 7 apply; a second role with `CREATE` on the
  schema but not owning `run_history` is refused ("must be owner of table
  run_history"). Migrations 6 and 7 are both new on this branch, so a
  deployment applies them together, as one role, in one transaction.

## Draining a namespace an older engine wrote (Task 9)

A namespace written before this change (aq-server's live namespace among
them) still holds runs that finished before the upgrade, with their
operations and receipts, `admin` receipts, events and keyed receipts with
no window or eviction-order entry, event tombstones, correlation fences,
disposal records, and on PostgreSQL legacy `disposal_tombstones` rows. The
drain moves all of it out as part of ordinary commits, a bounded amount per
commit, while new work is served. There is no offline step and no storage
format change. It needs no host call: the open commit of `Engine::new` and
every later commit that changes the State take one step
(`retention::drain_step`), timed or not; a licensed admin commit drains
nothing.

**Bounds** (policy fields, so a stored policy may set them; both must be
positive, `set_retention_policy` refuses 0 with "drain bounds must be
positive"):

| Field | Default | Bounds, per commit |
| --- | ---: | --- |
| `drain_cohorts_per_commit` | 50 | roots examined by the cohort scan, and settled cohorts moved to history (those the commit's own settlement moved count against it) |
| `drain_entries_per_commit` | 500 | legacy entries handled (tombstones, fences, disposal records, admin receipts, keyed receipts and events examined); also PostgreSQL legacy tombstone rows deleted |

**What it does, in order.** Progress is stored in `retention.drain`
(`DrainState`: `done`, `root_cursor`, `roots_done`, `phase`, `cursor`,
`legacy_sequence`), absent in an older State and then defaulted, so the
drain runs; a restart continues where it stopped.

1. *Cohorts.* The roots after `root_cursor` are examined; each settled one
   (members, none active, no unsettled operation) leaves as history, exactly
   as a cohort that settles now: one history row per run, effect and
   receipt, the run row's sequence its legacy terminal receipt's, so drained
   runs sort as the oldest in the rolling history and pass through its trim
   (with more than `history_keep` legacy runs the excess is trimmed as it is
   written: inspection keeps the newest `history_keep`). A live legacy
   cohort is skipped and leaves in the commit that settles it, like any
   other. All cohorts a commit removes share one compaction of the live
   receipts and one pass over the live waits.
2. *Entries,* one phase after the other: event tombstones and correlation
   fences are dropped; disposal records are cleared; `admin` receipts the
   older engine wrote go to history, each dated by its own time (receipts
   an admin action writes during the drain still leave with their own
   commit); each keyed receipt gets its expiry record (acceptance plus
   `admission_replay_ms`) and window key, and joins the eviction order, or
   `replay_held` while its run is live; each event gets its expiry record
   and window key, its `indexes.contract_events` entry, and joins the
   eviction order unless live work holds it. Each addition is skipped when
   already present, and each is the item's own key, as the eviction
   licence requires.
3. When both parts are complete, `drain.done` is set. From then on a commit
   pays one check.

**Re-arming.** A commit whose settlement finds more than 50 settled
cohorts sets `report.more` and re-arms the cohort scan, which picks up the
rest in later commits (none is stranded). An index rebuild (a State with
uninitialized indexes) re-arms the whole drain: the rebuild drops the
dirty sets, so cohorts settled before it are seen by no settlement.

**Correct while draining.**
- A duplicate of a legacy event or keyed admission is recognised as before
  until its entry leaves by the normal rules (window, then the rolling
  bound once it is in the eviction order). A redelivery of an event the old
  GC disposed of is refused while its tombstone is in the record, and
  accepted as new after the tombstone has left (as for any forgotten key).
  A wait registered while a correlation fence is still there is refused as
  before; after, it times out (ruling NR-1).
- Legacy live runs (a parent waiting on a child, joins, waiting runs,
  admitted runs) are untouched by the drain and settle normally.
- Inspection lists each legacy run exactly once at every commit: in the
  live State until its cohort is drained, in history from that commit.
- `collect_garbage` is an ordinary commit: it takes the drain's next
  step like any other (see "`collect_garbage` (Task 10)").
- **Operations dispatched by the old engine.** They carry no dispatch
  sequence, so without help an operation whose handle is lost would hold
  every event of its contract and, when its membership changed, judge all
  of them again in one commit (a day of events on aq-server). `Engine::new`
  fills the sequence once from the operation's own first `dispatch`
  receipt (an operation without one was never dispatched and holds
  nothing). And the events the older engine accepted are judged only by the
  drain, a bounded number per commit: until the drain has reached an event,
  a change of its contract's uncertain operations skips it. So no commit
  judges more than the bound of them (tested on bound record containers:
  the most any commit reads does not depend on how many such events there
  are). The fill of `contract_events` that `Engine::new` used to do in one
  pass over the events is part of the same bounded phase.

**PostgreSQL legacy tombstone rows.** Each commit (protected and plain
format 2, and both format 3 paths) deletes up to
`drain_entries_per_commit` rows of kinds `event`, `run` and `intent`
(`DELETE … WHERE ctid IN (SELECT ctid … LIMIT n)`, through the primary key),
after the identity-reuse check. Artifact rows stay (they guard artifact
bytes). The rows are their own progress record. When a commit deletes
fewer than its bound, the storage handle stops issuing the delete for that
namespace; a new handle (a restart) issues it once more. No engine writes
such rows any more.

**What an operator sees.** The drain starts by itself at the first open
after the upgrade. Each commit moves up to 50 finished runs to history and
handles up to 500 legacy entries. Measured (Task 9) on the 1,000-run
benchmark namespace (protected format 2, a 10.7 MB record): the drain was
complete at commit 21 after the open; those commits took 2.8, 3.7, 2.1 s,
then less as the record shrank (25.8 s in all), against about 0.6 s for a
step on that record before; then the record was about 104 KB and a step
about 30 ms. Meanwhile the run list keeps showing every run, and
`run_history` grows by the drained runs.

Legacy keyed receipts get their expiry from the admission window of the
policy stored when the drain reaches them (an older stored policy has none:
the default, 7 days). A window lowered afterwards does not shorten those.
They are still remembered only within `replay_keep`: newer keys push them
out, oldest first. It has finished when the State's
`retention.drain.done` is `true` (and `retention.drain` has no cursors),
the record holds no finished run, and no `disposal_tombstones` row of kind
`event`, `run` or `intent` is left for the namespace:

```sql
SELECT count(*) FROM aq_playbooks.disposal_tombstones
 WHERE namespace = $1 AND kind IN ('event','run','intent');
```

## With no limits

Nothing refuses work for the size of the work in progress itself, so that
size has a ceiling the engine does not guard. On protected format 2 (as
aq-server runs) the whole record is one encrypted value: every step
decodes, re-encodes, encrypts and rewrites it. So the time of every step
grows with the work in progress (runs in progress, their receipts,
operations and waits, and the events they hold), and the database's
on-disk churn grows with it. At PostgreSQL's 1 GB limit on one field (or
the process's memory, first) every commit fails, including those that
finish or cancel runs, since each rewrites the record: the namespace can
no longer move, and nothing inside the engine gets it out. This is a known
boundary, not a guarded one. The host sees it coming from the record size
and step time and can stop admitting work itself.

The two remedies on record: compress the snapshot before encrypting it
(record bytes, write volume and TOAST bloat shrink together; it needs a
version flag in the protected envelope), and per-item storage (format 3)
for protected namespaces, which has no single-value limit and writes only
what a step touches.

## PostgreSQL: on-disk size of the namespace table

On protected format 2 every transition rewrites the namespace's one wide,
encrypted row. Ciphertext does not compress, so each commit writes a new
TOAST value the size of the record and leaves the old one dead. TOAST space
is reclaimed only by vacuum, which makes it reusable without shrinking the
file. The on-disk size of `aq_playbooks.namespaces` is therefore a plateau
of roughly

```text
commit rate × record size × (autovacuum naptime + vacuum time)
```

and does not grow with the number of runs. At benchmark pace (about 1,500
commits a minute, a 124 KB record) that is a few hundred MB; at aq-server's
pace (tens of commits a minute) a few MB.

**Optional operator tuning** (no migration sets it; it needs ownership of
the table):

```sql
ALTER TABLE aq_playbooks.namespaces SET (
    autovacuum_vacuum_cost_delay = 0,
    toast.autovacuum_vacuum_cost_delay = 0
);
```

It makes each vacuum pass of that table finish in one go, which shortens
only the vacuum-time term. **It did not measurably change the size.** At
200 runs (10 s per run, 10-minute windows, protected format 2, default
autovacuum with a one-minute naptime) the table was 224.4 MB with the
setting, against 201.8 MB without it (one run each; 170.5 MB against
145–152 MB after the next vacuum pass). Autovacuum made one pass per naptime
either way: the naptime dominates. `autovacuum_naptime` is server-wide and
left to the operator.

The structural remedy on record is to compress the snapshot before
encrypting it on protected format 2. That shrinks the record bytes, the
write volume per transition and this plateau together.

## `collect_garbage` (Task 10)

`collect_garbage` is not needed, by anything: every commit already deletes
finished work and old entries, a bounded amount each. It stays callable so
existing callers compile, and the CLI `gc` command keeps working (its help
says it is not needed).

`Engine::collect_garbage(now_ms, limit)` is one ordinary commit at host time
`now_ms`: the same bounded settle, drain of a namespace an older engine
wrote, window drop and eviction every commit does (`finish_commit`), plus
its own small part (`retention::gc`): a page of at most `limit` artifacts
examined for disposal (`1..=policy.gc_page_limit`), catalog history no live
run, event or audit window needs, artifact disposals past `audit_ms`, and
the GC clock (`last_gc_at_ms`; a call before the previous one is refused).

It removes no run, operation, receipt or event itself, and writes no event
tombstone, correlation fence, disposal record or (PostgreSQL)
`disposal_tombstones` row of a run, operation or event. A finished run
leaves only with its history: on an older engine's namespace the call
advances the same drain, so its runs go to history.

`GcReport` (fields kept, two added with serde defaults):

| Field | Now |
| --- | --- |
| `runs` | finished runs this commit moved to history (settled, or drained) |
| `events` | events forgotten by the window drop |
| `admissions` (new) | keyed receipts forgotten by the window drop |
| `evicted` (new) | window entries evicted past `replay_keep` |
| `receipts` | receipts moved to history |
| `artifacts` | artifacts disposed of |
| `preserved` | artifacts kept because something references them (it no longer counts runs or events: those are not examined) |
| `more` | more bounded work is due: settled cohorts past the settle bound, an unfinished drain, expired entries past the drop bound, entries past `replay_keep` past the eviction bound, or the artifact phase |

Removed with it: GC's run removal without history and its event removal
(with tombstone, fence and disposal record); the disposal-record licence of
the removal fences (`retention::Removals`; `verify_removals` and
`check_removals` return `()`), on every backend and path; the PostgreSQL
tombstone writes for runs, operations and events and the per-handle re-arm
only they needed; the GC cutoff delete of run and operation rows; GC's
whole-record receipt reindex; and the writes of `indexes.root_last_activity`,
which only GC's age test read (the field still decodes; an entry an older
engine wrote leaves with its cohort). `RetentionState::{event_tombstones,
expired_correlations, disposals}` and an older engine's GC cursor still
decode, are never written, and are drained; while one is still in the
record it behaves as before (a tombstoned redelivery is refused, a fenced
wait is refused).

## Per-commit bounds are policy fields (Task 10)

The owner's rule: no hard-coded limits. Every bound of the deletion work a
commit does is a `RetentionPolicy` field with a serde default (the old
constant), so a stored policy decodes with or without it and may set it;
`set_retention_policy` refuses 0.

| Field | Default | Bounds, per commit |
| --- | ---: | --- |
| `window_drops_per_commit` | 64 | expired window entries examined (dropped or deferred) |
| `replay_evictions_per_commit` | 64 | entries evicted past `replay_keep`; the eviction licence reads the same field (of the State the commit writes) |
| `history_trim_per_commit` | 64 | history units a commit trims beyond what it writes, per rolling set, on every backend |
| `gc_page_limit` | 1,000 | the largest `collect_garbage` page, and the PostgreSQL artifact catalog delete of a GC commit |
| `settle_cohorts_per_commit` | 50 | settled cohorts that leave the State in the commit that settles them (the rest re-arm the drain) |
| `drain_cohorts_per_commit` | 50 | (Task 9) |
| `drain_entries_per_commit` | 500 | (Task 9) |

`window_drops_per_commit` also bounds the events a commit judges again
after a contract's uncertain operations change (see the hold lapse below).

Constants that remain: `REBASE_ADJUSTMENTS_LISTED` (1,000, how many
capped timers a clock-rebase receipt lists; an audit list size, not a
deletion bound), the index query page cap (1,000 items, a read bound), and
the per-row profile bounds of the catalog projection. The old constants
(`WINDOW_DROPS_PER_COMMIT`, `REPLAY_EVICTIONS_PER_COMMIT`,
`HISTORY_TRIM_RUNS_PER_COMMIT`, `GC_PAGE_LIMIT`, `COHORTS_PER_COMMIT`)
remain as the defaults.

## A lost handle's hold lapses (Task 10, ruling C)

An asynchronous operation whose dispatch lost its handle holds the later
events of its completion contract and scope (they may be its completion),
uncounted and never evicted. That hold now lapses once the retention clock
is past the operation's command deadline (`Intent::deadline_at_ms`) plus
the completion event's window (`max(event_replay_ms,
contract.retention_ms)`). After it, those events are ordinary unheld
entries: they rejoin the eviction order at their acceptance positions (so
as the oldest) and leave by `replay_keep` or by their own window.

- Each commit takes the lapsed operations out of the uncertain index
  (`retention::lapse_holds`; O(uncertain operations), nothing when no
  handle is lost), so `rejoin` judges the events they held again: in
  batches of at most `window_drops_per_commit` per commit, from
  `RetentionState::rejudge` (contract → the sequence to continue after;
  not written when empty), with `CommitReport::more` set until done. The
  same bounded queue serves every change of a contract's uncertain
  operations (a handle lost or found); a later change restarts from the
  earliest dispatch concerned. Until an event is judged it is never
  forgotten early: if it has become held, `evict` takes it out of the
  order instead of evicting it; if unheld, it waits outside the order or
  leaves by its window.
  `event_hold` applies the same rule, so the removal fences agree on every
  path.
- **The handle found before the lapse:** the held completion matches and
  the operation completes, as before.
- **The handle found after the lapse:** the completion may be gone. The
  operation is then past its command deadline, so the next drive marks it
  `Unknown` again ("operation deadline elapsed with unresolved external
  outcome") and it is in the recovery queue: a `Reconcilable` command is
  reconciled with the host, whose answer completes it; other recovery
  kinds need an operator's authorized recovery or a cancel, as for any
  operation whose outcome is unknown. (Tested both ways.)

## PostgreSQL: receipt index rows by sequence (Task 10)

On protected and plain format 2 the runtime-index rows of receipts
(`receipt`, `all_receipt`) are keyed by the receipt's sequence (20
zero-padded digits), not by its position in the record. A commit that
compacts the receipts (a cohort leaving, the drain) therefore writes the
rows of the receipts it adds or removes only; before, it rewrote the rows
of every later receipt (1.1 s of a 1.8 s drain commit on the 10 MB legacy
namespace, Task 9 review I1).

- **Rows an older engine wrote** (keyed by position, unpadded, so never
  equal to a new key) are not converted and need no migration: each is
  deleted by its sequence when its receipt leaves, and a commit that
  writes a receipt's row afresh deletes any other row of that receipt. The
  delete names the rows and finds each through
  `runtime_correlated_sequence` (one lookup per row) when the table has
  planner statistics; without them the planner uses the primary-key
  prefix, which reads all of a namespace's receipt rows per lookup, and no
  query shape avoids that. So **opening the storage analyzes the engine's
  tables that have no statistics** (after the migrations, outside and
  before any commit): each table never analyzed (`reltuples` < 0) or
  holding rows with no `pg_stats` entry, as a `pg_restore` leaves them
  (PostgreSQL 17 restores no statistics; its index builds set
  `reltuples`), gets one `ANALYZE` of its own. It takes SHARE UPDATE
  EXCLUSIVE (blocks no reader or writer) and reads a bounded sample: about
  150 ms in all on the restored 1,000-run dump (`runtime_index` 52 ms,
  `inspection_rows` 59 ms, the rest a few ms each; the listing 9 ms). A
  table with statistics costs one catalog query. It is best effort: a role
  that may not analyze a table it does not own gets a server warning, the
  table is skipped and the skip is reported on stderr; any error is
  reported and ignored. Such a role should have the owner run `ANALYZE`
  after a restore (autovacuum also does it within a naptime).
- `IndexQuery::Receipts` answers each item's `key` with the receipt's
  sequence (20 digits) on every backend (it was the receipt's position).

**Measured** on the 1,000-run legacy dump (`legacy-a-1000.dump`, protected
format 2, 10.7 MB record, 24,000 receipt rows keyed by position), the
Task 9 procedure under aq-server's session settings (`statement_timeout`
2 s, `lock_timeout` 500 ms), after `ANALYZE`:

| Commit | Task 9 | Now |
| ---: | ---: | ---: |
| 1 (open) | 2,815 ms | 802 ms |
| 10 | 1,410 ms | 452 ms |
| drain, commits 1–21 | 25.8 s | 9.3 s |

The drain completed at commit 21 as before; afterwards a step took about
27 ms. Without statistics and without the analyze at open (restored,
autovacuum off): commit 1 took 1,720 ms, commit 10 856 ms. With the
analyze at open, on a never-analyzed restored copy (autovacuum off for
every engine table): commit 1 671 ms, commit 10 303 ms, the drain 6.3 s.

## History table

`aq_playbooks.run_history` (migration 6), one row per inspection item:

| Column | Type | Meaning |
| --- | --- | --- |
| `namespace` | text, FK `namespaces` | |
| `kind` | text, `runs` / `effects` / `receipts` | the inspection kind |
| `item_key` | text `COLLATE "C"` | the item's inspection key (run id, operation id, receipt sequence) |
| `run_id` | text | the run; `admin` or `event:<key>` for receipts owned by no run |
| `root_id` | text | the cohort's root |
| `sequence` | bigint ≥ 0 | a run row: its terminal receipt's sequence (else its cohort's last receipt's); a receipt: its own |
| `finished_at_ms` | bigint | host time of the commit that removed it |
| `public_value` | jsonb | exactly what inspection returned for the item while it was live (`inspection::public_run` / `public_intent` / `public_receipt`) |

- Primary key `(namespace, kind, item_key)`; `run_history_by_run
  (namespace, kind, run_id, item_key)` serves one run's pages;
  `run_history_by_finish (namespace, finished_at_ms)` serves
  `delete_history_before`.
- Insert-only: an `UPDATE` trigger and a `TRUNCATE` trigger refuse; `INSERT`
  and `DELETE` need the session holding the namespace lease.
- History rows hold public projections only, never encrypted (protected
  inputs and results are never in them).
- Migration 7 adds the three partial indexes on `(namespace, sequence,
  item_key)` the rolling trim reads (runs; `event:%` receipts; `admin`
  receipts) and `run_history_counts`, so the trim never counts rows (see
  "History is a rolling log").
- A commit that would leave an item key both live and in history is refused
  at write on every backend, so inspection needs no de-duplication. Pages are
  bounded per branch (live, history) and merged by key.

## Format 3

Format 3 (per-item records) gets the same behaviour through the shared
transition code (`finish_commit`). Choices (SC-13, confirmed by Task 11's
parity test `format_3_matches_the_reference_for_settle_drop_stale_and_drain`):

- **Removing a cohort is a heavy commit.** `receipts` is a positional
  `LazySeq`: removing a cohort's receipts compacts it, and the commit takes
  full-container fallbacks on `receipts`, `indexes.receipts` and
  `indexes.all_receipts` (`retention.rs:1023`–`1069`), so the record store
  reads all records and writes the result through the complete path
  (`records.rs:517`). Making it light needs sequence-keyed receipt records,
  a schema and decoding change; the record is now small, so the builder kept
  it heavy (spec §7 allows it).
- **Window drops and evictions stay incremental**: `stage` proves each
  deleted event or keyed receipt against its deleted expiry record and the
  header clock, and the eviction licence runs while the transaction is still
  lent. An unproven deletion takes the heavy path.
- Admissions, events and steps that settle nothing stay incremental with no
  fallbacks.
- A licensed admin commit takes the complete path, where `verify_removals`
  judges the licence.
- Opening a namespace whose indexes are not initialized goes through the full
  load and compare-and-swap once.

## Acceptance results (Task 12)

**Environment.** One throwaway container, `engine-growth-task12`:
`postgres@sha256:266d3c257594e195b309ae9d1805e54c788f1d7b375f56bf6c367aac6c917e7b`
(PostgreSQL 17.11, alpine), `--cpus=2 --memory=1g --shm-size=128m`,
`-c shared_buffers=128MB -c fsync=on -c synchronous_commit=on
-c full_page_writes=on -c max_connections=32`, default autovacuum (naptime
1 min). Binaries: release builds of `5de3049`, `engine_growth` sha256
`5e724fb8031b8fdc48b4173e73bc8ac416f8a5a0361ac16000ce238fefb7642b`,
`engine_growth_drain` sha256
`7854f67b8f00352a62d87daf6416b811a81b98468099b96f19d601b238c9995b`. The
host (WSL2, 24 cores) was shared with other agents' builds and test
databases throughout; its load average went from 4 to 30 during the
run. Step times are wall-clock times on that machine. Raw output:
`.local/0003/` (not committed); verdicts and checkpoint summaries:
`0003-live-state-only.results.json`.

**What was run.** The owner asked for the project to be finished tonight,
and the machine was overloaded. So only acceptance 1 and 3 (the
10,000-run steady benchmark) and one 300-run control were run in this
task. Acceptance 2 (burst) and 4 (upgrade) were **not re-run**. The figures
below for them come from the earlier tasks on the same engine path (Task
8b fix round 1 for the burst, Task 10 fix round 1 for the drain), and they
are marked as such. The `burst` and `upgrade` modes are in the committed
benchmark and were only smoke-tested (30 runs).

### Acceptance 1: steady rate, 10,000 runs

`engine_growth steady 10000 <cfg_steady>`: protected format 2, run k at
k × 10 s, 10-minute event and admission windows, every admission and event
with its request time, `collect_garbage` never called. Medians are over each
checkpoint's last 20 runs (about 360 steps).

| Run | Record (`SIZE_SQL` total) | Namespace row | Median step | p95 | Max |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 11,306 B | 4,984 B | 19.6 ms | 29.4 ms | 29.4 ms |
| 200 | 168,914 B | 138,048 B | 23.6 ms | 31.7 ms | 90.5 ms |
| 500 | 169,154 B | 138,288 B | 23.6 ms | 30.1 ms | 42.6 ms |
| 1,000 | 169,274 B | 138,408 B | 22.6 ms | 28.0 ms | 36.9 ms |
| 2,000 | 169,458 B | 138,592 B | 27.1 ms | 41.7 ms | 428.4 ms |
| 5,000 | 169,698 B | 138,832 B | 28.5 ms | 36.2 ms | 61.8 ms |
| 10,000 | 169,818 B | 138,952 B | 27.9 ms | 36.1 ms | 43.3 ms |

Verdict line: `"acceptance":1,"pass":true`. Record ratio (10,000 / 200)
**1.005**; median ratio **1.183** (bound 1.2); `namespaces` on disk at
10,000 / 2,000 **0.74** (bound 3). Wall time 7,213 s (2026-11-14, about 06:25Z to 08:25Z).

**Record size: flat.** The record (`SIZE_SQL` total: the namespace row and
its runtime-index and inspection rows) is 168,914 B at run 200 and 169,818 B at run 10,000 (+904 B over 9,800 runs). The only growth is
the digits of counters.

**Step time: the verdict passes (1.18×), but the step time is not flat, and the 0.02 margin is within the machine noise.** Over the run:

| Runs | Median | p95 | p99 | Max | Steps > 1 s |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1–1,000 | 23.5 ms | 31.3 | 39.2 | 574.7 | 0 |
| 1,001–2,000 | 25.4 ms | 33.7 | 44.2 | 858.7 | 0 |
| 2,001–3,000 | 26.1 ms | 35.0 | 45.7 | 1,327.7 | 1 |
| 3,001–4,000 | 27.4 ms | 66.6 | 94.9 | 950.1 | 0 |
| 4,001–5,000 | 27.7 ms | 37.5 | 56.7 | 416.9 | 0 |
| 5,001–6,000 | 27.6 ms | 39.0 | 54.7 | 1,639.1 | 1 |
| 6,001–7,000 | 27.5 ms | 38.0 | 54.1 | 2,195.5 | 3 |
| 7,001–8,000 | 28.4 ms | 55.2 | 100.4 | 1,169.0 | 2 |
| 8,001–9,000 | 31.0 ms | 76.2 | 115.8 | 2,400.5 | 3 |
| 9,001–10,000 | 29.0 ms | 42.3 | 63.1 | 2,052.6 | 5 |

180,000 timed steps (18 per run): first run median 19.6 ms, overall median
27.2 ms, last run median 27.0 ms, worst 2,400.5 ms (run 8,471, under the
peak host load), p99 83.0 ms. The 15 steps over 1 s are single outliers
spread over the run, none consecutive.

What the numbers show:

- The median rose from 23.5 ms (runs 1–1,000) to about 27.6 ms by run
  4,000, then stayed level through run 7,000. It rose again from run 7,000,
  while the host's load average was 10–30 (CPU pressure up to 25 %).
- **Control.** A fresh namespace (`steady 300`, its own database in the
  same container) ran *at the same time* as steady runs 8,981–9,255.
  Fresh namespace, runs 61–300: median **27.6 ms**. The 9,000-run namespace
  over the same minutes: **30.8 ms**. The same fresh setup alone at the
  start of the night: 24.0 ms.
- So about 4 ms of the rise is the machine (the fresh namespace was 15 %
  slower at 08:12Z than the steady run's first 300 runs at 06:25Z). About 3 ms (**1.12×**) is tied to the
  namespace that has served 9,000 runs, and load does not explain it.
- Where that 3 ms comes from is **not settled**. The rise is the same in all
  18 commits of a run (not only the settling commit that writes history).
  The record and the WAL per commit (161 KB) are flat. The history-overlap
  check is an index probe (`EXPLAIN`: nested loop over
  `run_history_pkey`). The leading suspect is the database working set:
  `run_history` (106.5 MB at run 10,000) plus the namespace TOAST (200–320
  MB) outgrowing the container's 128 MB `shared_buffers` and 1 GB memory.
  That would make page reads and checkpoint writes per commit dearer. If so,
  it stops growing once history reaches `history_keep` (10,000 runs, this
  benchmark's end). That is not shown: no run went past `history_keep`.
- In a 15 s statement log at run 5,200, `COMMIT` (WAL fsync) was 55 % of
  server time (5.5 ms per commit); the next was the namespace row `UPDATE`
  (1.0 ms).

### On-disk size (protected format 2, one namespace per database)

Per table, heap / TOAST / indexes / total, MB:

| Run | `namespaces` | `run_history` | `runtime_index` | `inspection_rows` | All engine tables |
| ---: | --- | --- | --- | --- | ---: |
| 200 | 0.0 / 276.7 / 0.0 / **276.7** | 1.2 / 0 / 1.0 / **2.2** | **2.6** | **0.7** | 282.5 |
| 2,000 | 0.0 / 271.2 / 0.0 / **271.3** | 12.2 / 0 / 9.1 / **21.3** | **3.3** | **1.3** | 297.5 |
| 5,000 | 0.0 / 204.3 / 0.0 / **204.4** | 30.7 / 0 / 22.6 / **53.3** | **3.2** | **1.0** | 262.2 |
| 10,000 | 0.0 / 202.0 / 0.0 / **202.0** | 61.4 / 0 / 45.1 / **106.5** | **3.2** | **1.3** | 313.4 |

`namespaces` TOAST autovacuum passes: 2 by run 200, 21 by 2,000, 57 by
5,000, about 120 by 10,000. `run_history` held 149,975 rows (10,000 runs,
20,000 effects, 119,975 receipts) at run 10,000.

- **`namespaces`: a plateau, not growth.** Its TOAST holds dead copies of
  the rewritten record until autovacuum reuses them. The table peaked at
  about 320 MB around run 500–1,000 and then shrank, to 202.0 MB at
  run 10,000. That is 0.74× its size at run 2,000; the ruling's gate
  is 3×.
- **`run_history` grows with total runs, by design, up to `history_keep`.**
  It holds every finished run's items (15 rows a run in this playbook:
  about 10.7 MB per 1,000 runs, indexes included). With the default
  `history_keep` 10,000 it reaches its ceiling exactly at this benchmark's
  last run. From then on each commit that writes a run trims one. The trim
  itself is tested (`history_is_a_rolling_log_on_postgres`, the trim
  `EXPLAIN` test), but **this benchmark does not show the on-disk plateau
  past `history_keep`.** Deleted rows leave dead tuples for autovacuum to
  reuse, so the file should level at about the 10,000-run size; that is not
  measured.
- `runtime_index` and `inspection_rows` stay at 3.2 MB and 1.0–1.3 MB.

### Acceptance 3: history after the steady run

`"acceptance":3,"pass":true`: paging `Runs` 1,000 at a time listed
10,000 items with 10,000 distinct ids, exactly the 10,000 runs served; every
item equals the projection (`inspection::public_run`) captured in the commit
that removed the run (0 mismatches); no live run left. The control's 300-run
namespace passed the same check.

### Acceptance 2: burst (not re-run; Task 8b fix round 1, same engine path)

1,000 runs with the clock frozen, protected format 2, 10-minute windows,
default `replay_keep` 256: the namespace row reached 287.6 KB at run 200,
287.9 KB at run 500 and 288.0 KB at run 1,000. It **levels** at 256
remembered entries (128 events and 128 keyed receipts), about 284 B per run
averaged over the burst (bound 2,048 B). Earlier, at 2,014 B per run
without the rolling set, it never levelled. In that run nothing live was
left after the burst. The on-disk TOAST plateau was 704 MB (burst pace,
default autovacuum). The NR-2 sequence and verdict lines were added to the
benchmark in this task; they were exercised only in a 30-run smoke run. There
the record after the NR-2 sequence was 1.003× run 1, and the literal "after
20" figure was 5.5×, as NR-2 predicted.

### Acceptance 4: upgrade (not re-run; Task 10 fix round 1, same engine path)

On `legacy-a-1000.dump` (sha256 `ad8ef8ea…2093a`, 1,000 finished runs
written by the engine before this spec, a 10.7 MB record), never analysed,
under aq-server's session settings (`statement_timeout` 2 s, `lock_timeout`
500 ms):

- the drain finished at commit 21 (6.3 s in all; commit 1 671 ms, commit
  10 303 ms) while 200 new runs were served;
- all 1,000 legacy runs were in history;
- afterwards a step took about 27 ms.

Task 9 noted that the record then stays above the steady size for up to 7
days of host time: the open commit gives up to `replay_keep` legacy keyed
receipts the stored default 7-day admission window before the host lowers
it. This is bounded by `replay_keep`, and it does not grow. The committed
`engine_growth_drain upgrade` mode checks it (`record_bytes_max_after_done`
against 1.2 × the steady bytes) but was not run in this task. **This open
question is not measured: whether acceptance 4's "within 1.2× once
`drain.done`" holds.** By the Task 9 measurement (188 KB against 133 KB) it
would not hold until those receipts leave.

### Acceptance 6: test suites

Not re-run in this task (the owner's instruction to finish tonight;
the machine at load average ~30). The tree differs from `6bbbcdb`, where
Task 11 ran them all (workspace 311 passed; PostgreSQL suites 91 passed,
1 ignored), only in the two benchmark examples and the version numbers.
Checked here: `cargo check --locked --workspace --all-targets` passes,
`cargo fmt --check` is clean, and `cargo clippy --locked -p
aq-playbooks-postgres --examples -- -D warnings` is clean.

### Acceptance 5

The aq-server 500-run routing scale test runs in aq-server after the
re-vendor (server follow-on).

## Known boundaries and follow-ups

Accepted, not scheduled:

- **A very large live record on protected format 2.** Nothing bounds the
  work in progress itself (see "With no limits"). The whole record is one
  encrypted value: at PostgreSQL's 1 GB field limit (or process memory
  first) every commit fails, including those that would finish or cancel
  runs. The owner accepted this boundary for now ("address it if we run into
  the issue").
- **Compress the snapshot before encrypting it** on protected format 2: record
  bytes, write volume per commit and the TOAST plateau shrink together. Needs
  a version flag in the protected envelope; compression leaks some length
  information.
- **Write receipts to history as each step completes**, not only when the
  cohort settles, so a long-running run does not carry all its receipts in
  the record.
- **Per-item storage (format 3) for protected namespaces**: no single-value
  limit, and a step writes only what it touches.
- **The re-judge scan skips not-yet-drained legacy events without counting
  them against its budget** (`rejoin`): bound the iteration, not only the
  events judged. Only reachable while an older namespace is still draining.
- **A role that cannot `ANALYZE`** an engine table it does not own gets the
  skip reported and retries on every connect (about 11 cheap catalog and
  `ANALYZE` statements; connects happen at host start). Optional: report once
  and stop until the next restart. Have the owner analyze after a restore.
- **Analyze at open vs a concurrent autovacuum under a lock timeout**:
  `ANALYZE` waits for a SHARE UPDATE EXCLUSIVE holder such as autovacuum;
  under a `lock_timeout` (aq-server: 500 ms) it then fails, is reported and
  is ignored, and the open continues without fresh statistics. Judged not
  reachable for aq-server in practice.

- **Step time on a namespace with a long history (open).** At about 9,000
  runs a step took 1.12× a fresh namespace's step on the same machine at the
  same time (see acceptance 1). The cause is not identified; the suspect is
  the database working set (`run_history` growing to `history_keep`). A run
  well past `history_keep` on a quiet machine, with the storage profile on,
  would settle it.
- **Acceptance 2 and 4 were not re-run at 0.2.0**: the `burst` and
  `engine_growth_drain upgrade` modes exist for that.

Also carried from earlier tasks:

- A legacy keyed receipt gets its window from the policy stored when the
  drain reaches it; the open commit of an older namespace drains under the
  stored (default 7-day) admission window, so up to `replay_keep` such
  receipts are remembered until newer keys push them out or 7 days pass
  (see acceptance 4).
- On-disk size of `aq_playbooks.namespaces` on protected format 2 is a TOAST
  plateau set by the commit rate and the autovacuum naptime (see the
  measurements).
- The server follow-on (re-vendor, `requested_at_ms` on `Request::Event` and
  `Request::AdmitOnce`, the `Stale` arm, the 10-minute policy) is listed in
  the plan's "aq-server follow-on"; acceptance 5 (the 500-run routing scale
  test) runs there.

## Rewritten tests

Every test that asserted finished runs (or a removed mechanism) in the
State, with its old and new assertion (spec acceptance 6). Format:
test — old assertion -> new assertion. Task 11 rewrote none.

### Task 5 (settled cohorts leave the record as history; commits 77cb22d, 3b99833)

`crates/aq-playbooks-runner/tests/semantics.rs` (engine() now MemoryStorage::retaining_settled(); helpers settled(), inspected(), inspected_run())

- event_before_wait_restart_stale_revision_and_backlog_deadline — s.runs[id].outcome == ok after a third advance; s.waits winner -> wait winner checked while the run is live; third advance is NotFound; settled run outcome ok; waits empty
- expiry_wins_over_late_event_and_identity_conflicts — snapshot runs[id].outcome == error -> run left the State; inspection (history) outcome == error
- durable_intent_replay_keeps_id_business_retry_creates_new_id — runs[id].outcome ok; s.intents.len()==2 -> settled run outcome ok; live intents empty; history effects of the run == 2
- all_three_child_modes_are_atomic_and_independent_cancellation_survives — runs[id] terminal, runs[child] Completed -> both settled (left), one cohort
- owned_operation_join_uses_exact_pinned_completion_and_retained_admission — runs[id].outcome ok -> settled run outcome ok
- timeout_fallback_waits_for_truthful_owned_operation_cancellation — runs[id].outcome error -> settled outcome error
- exhausted_step_budget_finishes_once_without_reentering_error_graph — runs[id] Failed; 1 engine_failure receipt in State; 10x advance == Ok(false); receipt count unchanged -> settled Failed; 1 engine_failure receipt in history; 10x advance == NotFound; history receipts unchanged
- history_budget_stops_without_disposing_existing_records — runs[id] Failed with "history" reason -> settled run Failed with that reason (artifact still retained)
- operation_join_deadline_uses_ingress_time_even_when_processing_is_late — runs[id].outcome; join branch receipt from State -> settled outcome; branch receipt from the settled cohort
- missing_dependency_destination_is_durably_blocked_then_resumes_exact_bytes — all live runs Completed (now vacuous) -> State has no runs; history has 2 runs, all completed
- iteration_failure_policy_skips_or_stops_without_resetting_captured_membership — runs[id] outcome/loops/variables -> settled run's
- command_deadline_refuses_unissued_effect_and_preserves_issued_uncertainty — runs[first].outcome error -> settled
- retry_allowances_are_scoped_to_full_loop_invocation_paths — runs[id] outcome/attempts; budgets[id].iterations==2 after finish -> budget 2 checked while live (after drive 10); settled outcome/attempts; budget gone after finish
- bounded_driver_uses_persisted_fair_scope_queue_and_noop_does_not_commit — terminal scopes from live runs -> from settled cohorts; 4 live runs remain (store now retaining_settled)
- activation_controls_separate_calls_events_and_current_execution — runs[id].reason != revoked -> settled run reason != revoked, outcome ok
- gc_preserves_bound_transitive_closure_then_collects_terminal_cohorts_in_pages — runs.len()==3 before GC; GC empties runs -> State empty without GC; settled cohort of 3 runs; 3 history runs; GC pages remove 0 runs and keep the closure
- unsafe_recovery_requires_audited_request_and_preserves_original_authority — runs outcome ok; intents[id].authority; recovery_requests[req].consumed after finish -> consumed checked live after recover; settled outcome ok; settled intent authority; authorized_recovery receipt in history; recovery request left with its operation
- pruned_general_wait_history_refuses_registration_and_captured_replay_horizon_survives_policy_change — runs outcome error; "history expired" error receipt in State -> settled outcome; error receipt in the settled cohort
- cancellation_dispatch_crashes_reconcile_owner_before_same_key_delivery_retry — runs[id] Cancelled; cancellation key of State intent -> settled Cancelled; key of settled intent
- gc_preserves_possible_async_completion_while_lost_admission_handle_is_unknown — runs[id].outcome ok -> settled
- producer_epoch_compaction_keeps_history_fences_for_existing_invocations — runs[id].outcome error -> settled
- confirmed_async_admission_recovery_resumes_accepted_branch_before_completion — final runs[run].outcome ok -> settled
- accepted_recovery_does_not_uncancel_a_run_or_bypass_revoked_authority — final runs[run] Cancelled -> settled

`crates/aq-playbooks-runner/tests/review_regressions.rs` (engine() retaining_settled; helpers settled(), settled_runs())

- t4_transitive_dependency_bytes_stay_pinned_after_activation_and_restart — live runs Completed with old artifact/deps -> State empty; settled runs with old artifact/deps
- t18_receipts_default_deny_result_fields_and_refuse_protected_inputs_before_intent — State receipts lack the secret (now vacuous) -> settled cohort receipts (incl. command_result) and history inspection items lack it
- t26_nested_captured_iteration_consumes_shared_budget_and_restores_lexical_roots — budgets[id].iterations==5 after finish; run outcome/variables -> budget read while live (step-wise drive) == 5; settled outcome/variables
- named_heterogeneous_join_results_feed_a_typed_command_and_return — runs[id] status/output/variables; State intent input -> settled run's; settled cohort intent input
- catalog_dependencies_select_compatible_versions_once_then_pin_across_restart — live roots Completed; children artifacts from State -> State empty; settled roots and children
- retained_event_before_wait_is_driven_after_restart_without_manual_wakeup — runs[id].outcome ok -> settled
- completed_children_before_join_remain_drivable_after_restart — runs[id].outcome ok -> settled
- completed_cohort_gc_removes_ready_timer_and_dependency_index_residue — residue empty after GC -> residue (plus root/intent/run-root indexes) empty right after the settling commit, and still after GC
- execution_revocation_blocks_prepared_dispatch_without_discarding_the_intent — runs[id].outcome ok -> settled
- t17_quota_refusal_retains_prior_event_artifacts_and_destinations — only the store (with_config) switched to retaining_settled; assertions unchanged

`crates/aq-playbooks-runner/tests/keyed_admission.rs` (engines retaining_settled)

- gc_and_direct_storage_cas_cannot_forget_or_rebind_a_key — runs empty only after GC -> run already left as history before GC (settled run Completed); receipt kept; then GC as before

`crates/aq-playbooks-runner/tests/transition_equivalence.rs` (reference engine retaining_settled)

- check() (both tests) — State/revision/quota/index parity -> plus Runs/Effects/Receipts inspection parity (live + history) at every step
- transition_reference_equivalence — added: more than one settled cohort after the scenario

`crates/aq-playbooks-runner/tests/catalog_inspection.rs` — engines only switched to retaining_settled; assertions unchanged

`crates/aq-playbooks-postgres/tests/transition_cost.rs`

- transition_cost_flat_with_history (and the postgres variant) — histories were h live finished runs; zero full commits and zero fallbacks for every op -> histories are h settled cohorts written as run history (plus h events); state.runs empty; admit/event: zero full loads, zero full commits, zero fallbacks (delta), bounded path; dispatch settles exactly one cohort, zero full loads, full commits == settling commits on f3 (heavy, SC-13) and 0 on RecordStorage; record reads flat across 0/32/128/512; inspection parity added

`crates/aq-playbooks-postgres/tests/protected_indexes.rs`

- actual_engine_requires_protection_before_admission_and_encrypts_sensitive_dispatch_and_output — runs[root] outcome/private variables after drive; restored intent result decrypts -> private variable and intent result checked while the run is live (step-wise), restored engine decrypts them, then drives to finish; run left; history run outcome ok; history receipts/effects carry no protected sentinel

`crates/aq-playbooks-postgres/tests/keyed_admission.rs,` persistence.rs — no change needed (no engine-finished runs asserted)

`crates/aq-playbooks-cli/tests/host_pilots.rs` (setup retaining_settled; driver treats NotFound from advance as no progress)

- routing_uses_authorized_assessment_and_host_revision_preconditions — runs[root] outcome; State intents -> settled root; settled intents
- watcher_reconciles_host_attested_observation_and_recovers_atomic_notifications — runs[root] outcome -> settled
- watcher_partial_positive_scan_keeps_unseen_episode_open — runs[root] outcome -> settled
- coding_admission_and_completed_review_with_changes_cannot_publish — runs[root] escalated; State intents -> settled; settled intents (non-empty)
- coding_requires_explicit_accepted_review_and_exact_check_before_independent_followup — runs[root] success; independent run from State -> settled root; settled runs
- coding_real_publication_conflict_observes_new_base_repairs_and_checks_new_candidate — runs[root]; publish intents from State -> settled
- report_deadline_cancels_owned_author_and_late_completion_keeps_one_message — after late completion: runs[root] success and work.ensure intent has late_results -> root settled after the deadline fallback with the author op Cancelled+cancel_requested in history; the late completion finds no operation (SC-3 ruling), revives nothing, history unchanged; one message

`crates/aq-playbooks-cli/tests/cli.rs`

- offline_cli_round_trip_and_invalid_input_exit — added: simulation report inspection lists the finished runs (all terminal) and receipts from history

`crates/aq-playbooks-cli/tests/admin_cli.rs`

- protected_admin_lifecycle_control_recovery_rotation_and_paged_inspection — line 201's run view now comes from storage inspection (CLI change, no test change); added: cancel -> host settles the operation -> cancel again finishes the run, which left the State; view (history) shows it cancelled; a further cancel fails (NotFound)

### Task 6 (window entries with expiry, dropped 64 per commit)

`crates/aq-playbooks-runner/tests/semantics.rs`

- pruned_general_wait_history_refuses_registration_and_captured_replay_horizon_survives_policy_change -> renamed expired_general_wait_history_times_out_and_captured_replay_horizon_survives_policy_change — old: redelivery at 1001 (after the captured 1_000 horizon) refused; GC; a wait registered at 2001 refused with an "history expired" error receipt (outcome error at 2001). New (NR-1 ruling, spec §3): redelivery at 999 is a duplicate (captured horizon outlives the 1 ms policy); at 1001 it is new; at 2001 the event has left with no tombstone or correlation fence; the wait registered at 2001 is Waiting, times out at 2102 (outcome error), no waits left
- producer_epoch_compaction_keeps_history_fences_for_existing_invocations -> renamed producer_epoch_retirement_leaves_a_wait_after_an_expired_event_to_time_out — old: GC at 2000 wrote correlation fences, epoch retirement kept them (live roots), the wait at 2001 was refused (outcome error at 2001). New (NR-1): the event left at its window in the GC commit's drop, so events, expired_correlations and event_tombstones are empty after retirement; the wait at 2001 is Waiting and times out at 2102 (outcome error)

### Task 8 (quotas count live work; max_tombstones and terminal_run_ms ignored)

`crates/aq-playbooks-postgres/tests/protected_indexes.rs`

- disposal_quota_refuses_atomically_and_expired_unreferenced_artifact_metadata_is_collected -> renamed max_tombstones_is_ignored_and_expired_unreferenced_artifact_metadata_is_collected — old: a commit writing 2 disposal tombstone rows with policy.max_tombstones = 1 was refused ("durable disposal tombstone quota reached"), state unchanged; then retried with max_tombstones 10 and last_gc_at_ms 100. New (spec §4, max_tombstones ignored): the same commit with max_tombstones = 1 (and last_gc_at_ms 100, audit_ms 1) is accepted and stored as written; 2 tombstone rows; the artifact half is unchanged (GC clock 102 deletes both tombstone rows and both artifact_catalog rows). For Task 12: the refusal half is gone by design.

`crates/aq-playbooks-runner/tests/transition_equivalence.rs`

- transition_reference_equivalence (scenario) — added, not rewritten: two quota refusals (max_receipts = live receipts + 1, then max_state_bytes = current bytes) compared on MemoryStorage and RecordStorage; the refusal texts carry the counts, so the maintained accounting must equal quota_counts there. Existing steps unchanged.

### Task 8 fix round 1 (a quota refuses only growth; migration 7 dropped)

`crates/aq-playbooks-runner/tests/live_state.rs` (Task 8's own tests)

- a_quota_refusal_names_what_is_full — runs and receipts texts now "runs in the record (in progress, or finished and not yet moved to history)" and "receipts: those of runs in the record, and keyed admission receipts inside their replay window or kept while their run is live"
- a_legacy_namespace_at_its_run_quota_still_commits_the_work_that_drains_it — matches "53 runs in the record"

`crates/aq-playbooks-postgres/tests/live_state.rs`

- namespaces_table_and_its_toast_vacuum_without_cost_delay — removed with migration 7 (ruling I1)

`crates/aq-playbooks-runner/tests/transition_equivalence.rs` (scenario, added)

- after the i64::MAX pin: policy change, rebase_retention_clock(30_000), three keyed admissions, a keyed run on "child" that stays live (deferred), drive at 30_010, admission at 31_500 (window drop); then max_runs = live - 2 on both: an admission is refused, a drive lands and lowers the count

### Task 8 fix round 2 (limits gate admission; in-flight work has a hard ceiling)

`crates/aq-playbooks-runner/tests/live_state.rs` (Task 8's own tests)

- a_quota_refusal_names_what_is_full, over_a_lowered_limit_commits_that_do_not_grow_the_count_land, a_legacy_namespace_at_its_run_quota_still_commits_the_work_that_drains_it — refusal texts now "the namespace is full for new work: N runs in the record (quota max_runs M)" etc.
- a_clock_rebase_lands_at_a_full_state_record — now also with the record at its hard ceiling (limit = bytes × 8 / 9), where only the licence's exemption lets it land

`crates/aq-playbooks-runner/tests/transition_equivalence.rs` — refusal step text "the namespace is full for new work"; scenario adds 8 keyed runs, then at full max_receipts an admission (new work, refused), at max_receipts 1 a cancel (in flight, refused at the ceiling), at full a cancel (lands), on both paths

`crates/aq-playbooks-postgres/tests/live_state.rs` — a_quota_refuses_only_growth_on_postgres: "(quota max_runs 1)"; rebase at the ceiling

### Task 8b commit 1 (owner override: no namespace limits; the quota mechanism is removed)

Reason for every entry below: spec 0003 final section "no limits" — nothing is ever refused because a namespace holds too much; the deprecated limit fields are accepted and ignored.

`crates/aq-playbooks-runner/tests/live_state.rs` (Task 8's quota tests)

- max_runs_counts_live_runs_only — removed (asserted a max_runs refusal); replaced by a_thousand_root_runs_in_progress_are_all_accepted_then_all_finish (1,000 root runs, direct and keyed, in progress at once under max_runs 1, all accepted, all finish)
- max_receipts_counts_the_window_and_frees_when_it_passes — removed (asserted a max_receipts refusal); covered by nothing_is_refused_for_namespace_capacity (a run holding >1,500 receipts under max_receipts 1)
- a_quota_refusal_names_what_is_full — removed (the refusal no longer exists)
- a_legacy_namespace_at_its_run_quota_still_commits_the_work_that_drains_it -> renamed a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains — old: admission refused ("53 runs in the record"); new: the admission at max_runs 52 with 53 runs in the record lands; the live and the new run finish; GC still moves the 51 legacy runs out
- a_clock_rebase_lands_at_a_full_state_record — removed (tested the licence's quota exemption, which is gone with the quota); rebase coverage unchanged elsewhere, and nothing_is_refused_for_namespace_capacity_on_postgres rebases under max_state_bytes 1
- over_a_lowered_limit_commits_that_do_not_grow_the_count_land — removed (asserted refusals over a lowered limit)
- at_a_full_receipt_quota_admitted_runs_drive_to_completion, at_a_full_receipt_quota_a_waiting_run_can_be_cancelled_and_can_time_out, at_a_full_state_record_registering_a_wait_lands, work_already_admitted_is_refused_past_the_hard_ceiling — removed (each asserted a quota or ceiling refusal); the progress they protected is asserted by nothing_is_refused_for_namespace_capacity and a_thousand_root_runs_... under the smallest limits
- a_namespace_at_its_configured_state_bytes_opens_and_takes_its_first_commits -> renamed a_pre_d10_namespace_opens_and_takes_its_first_commits_whatever_its_configured_limits — old: open and first commit at max_state_bytes = size and at the ceiling (bookkeeping exemption); new: the same open and first commits with every limit at its smallest (no exemption exists to test)
- new_events_artifacts_and_activations_are_new_work — removed (asserted max_events / max_state_bytes refusals); events with fan-out, install and activate under the smallest limits are in nothing_is_refused_for_namespace_capacity
- added: calling_parents_complete_whatever_max_runs_says (Task 8 re-review round 2 deadlock reproduction kept as a regression test), deprecated_limit_fields_decode_and_change_nothing
- TransitionRequest closures (refuses_live_and_history_conflicts, record_storage_transitions_hold_the_removal_fences): the `validate` argument is gone with the field

`crates/aq-playbooks-runner/tests/keyed_admission.rs`

- full_quotas_refuse_new_atomic_admission_without_blocking_exact_lookup -> renamed deprecated_limits_refuse_no_admission_and_replay_stays_read_only — old: a new key refused at full max_receipts; new: admitted; the exact replay is still a read-only lookup (revision unchanged)

`crates/aq-playbooks-runner/tests/transition_equivalence.rs`

- Pair::check — the per-step assert "maintained quota accounting == quota_counts" became "maintained record counts == the complete State's container sizes" (the byte and queued-destination sums are gone)
- scenario — removed: the two refusals at max_receipts = live + 1 and max_state_bytes = current bytes, the over-lowered-max_runs refusal, and the both-gates block (new work refused at full, cancel refused at the ceiling); added: with every limit at its smallest, 8 keyed admissions, a parent (child start), an event and a cancel land on both paths, drive lands

`crates/aq-playbooks-postgres/tests/live_state.rs`

- a_quota_refuses_only_growth_on_postgres and limits_gate_admission_not_progress_on_postgres — removed (asserted refusals); replaced by nothing_is_refused_for_namespace_capacity_on_postgres (f2 protected and f3, every limit at its smallest: 20 keyed and 3 invoke admissions land and drive to completion, a pinned clock is rebased)
- TransitionRequest closures: the `validate` argument is gone

`crates/aq-playbooks-postgres/tests/transition_cost.rs`

- compare(): "maintained quota accounting == quota_counts" became "maintained record counts == the complete State's container sizes"

`crates/aq-playbooks-postgres/examples/operating_envelope.rs`

- the quota_refusal scenario (asserted refusals) is removed

`crates/aq-playbooks-runner/tests/review_regressions.rs`

- t17_quota_refusal_retains_prior_event_artifacts_and_destinations -> renamed t17_refused_event_retains_prior_event_artifacts_and_destinations — old: a second event refused at max_events 1, state byte-identical; new: a refused event (forged producer) leaves the state byte-identical, the second event past max_events 1 is admitted, the first event's artifacts and destination remain and fan out

### Task 8b commit 2 (R1: rolling repeat protection, replay_keep)

`crates/aq-playbooks-runner/tests/live_state.rs`

- burst_at_a_frozen_clock_leaves_at_most_2_kb_of_window_entries_per_run — added `replay_keep: 1_000` (above its 400 entries). Reason: with the default 256 the rolling bound now evicts the oldest (it left 128 keyed receipts and 128 events), so the test would no longer measure the bytes of every window entry; its assertions are unchanged.

`crates/aq-playbooks-postgres/tests/transition_cost.rs`

- histories() (fixture of transition_cost_flat_with_history and its postgres variant) — sets `replay_keep: 1_000` before growing the fixture. Reason: at history 512 the default 256 evicts events by count, and the fixture asserts (and the cost comparison needs) all h events still remembered; assertions unchanged.
- added (not rewritten): transition_equivalence scenario — after the no-limits block, replay_keep 3 and history_keep 6 on both paths: the lowered limit drains through drive, then 5 keyed runs and 5 events each evict the oldest; order and history compared at every step

### Task 8b commit 3 (R2: rolling history, history_keep)

- no existing test rewritten. Added runner: memory/record_storage_history_keeps_the_newest_runs, memory/record_storage_trim_keeps_up_with_one_commit, history_keep_zero_writes_no_history, memory/record_storage_delete_history_and_the_trim, a_page_across_a_trim_neither_repeats_nor_panics; postgres: history_is_a_rolling_log_on_postgres, the_trim_keeps_up_with_one_commit_on_postgres, history_tests::history_trim_reads_and_deletes_through_indexes

### Task 8b fix round 1

`crates/aq-playbooks-runner/tests/live_state.rs` and crates/aq-playbooks-postgres/tests/live_state.rs

- rebase_refusal_cases "rebase that also changes an event's contract to shrink its cap" — expected text REFUSE_EXPIRY -> REFUSE_EVENT_CHANGE. Reason (review C1): an accepted event's envelope is now immutable on every path, so the commit is refused by that fence before the cap is judged; still refused, nothing written.
- the eviction fence fixtures (hold_by_waiter) now build a real waiting run and wait record (review C2: holds are judged from live records); the seeding clears `waits` with the runs.
- receipts_of_no_run_are_rolling_too counts the 5 still-remembered events' live receipts beside the history bound (added test, corrected while writing).

### Task 8b fix round 2 (re-review round 1: the uncertain re-judge)

`crates/aq-playbooks-runner/tests/live_state.rs`

- uncertain_completion_holds_are_tracked_both_ways — rewritten. Old (round 1): events accepted before the async dispatch were held by its lost handle. New (re-review ruling): an uncertain operation holds only events accepted after its dispatch; the earlier events stay in the eviction order (and are pushed out), events accepted while the handle is lost are held and rejoin, as the oldest, when it is found.
- the eviction fence fixture uncertain_event_state: the held event is accepted at sequence 1 after an operation dispatched at sequence 0 (it must come after the dispatch to be held).
- Intent literals in test fixtures (runner live_state, postgres tests/support, cli tests catalog_contract, catalog_v2_contract, catalog_v2_1_contract) gain `dispatch_sequence: None` (new optional field).
- added: an_async_dispatch_rejudges_no_earlier_event

### Task 9 commit 1 (dispatch sequences; Task 8b re-review round 2)

- no existing test rewritten. async_artifact() now delegates to the new async_artifact_with(Recovery) (same artifact). Added: a_recovery_redispatch_keeps_the_hold_on_a_completion_accepted_while_the_handle_was_unknown, an_operation_never_dispatched_holds_no_event, a_legacy_dispatched_operation_gets_its_dispatch_sequence_on_open

### Task 9 commit 2 (the drain)

`crates/aq-playbooks-runner/tests/live_state.rs`

- a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains — old: after the legacy State (indexes not initialized) was written, the 51 legacy runs stayed until collect_garbage removed all 51 (without history). New: the commit that rebuilds the indexes re-arms the drain, the drain moves them to history (50 then 1), collect_garbage removes none (it leaves legacy runs to a drain in progress); all 51 are in history. The no-limits point (new work admitted whatever max_runs says) is unchanged.
- the_engine_takes_held_entries_at_the_head_out_of_the_order — fixture only: both hand-built States are marked drained (retention.drain done). Reason: a State with no drain record is drained on open, and the drain would judge the entries the test places at the head (it takes a held one out and puts an unheld one in) before evict can be observed. Assertions unchanged.

`crates/aq-playbooks-postgres/tests/live_state.rs`

- no assertion changed: legacy_tombstone_rows_beyond_max_tombstones_refuse_no_commit_and_are_not_scanned and full_commit_reads_only_created_identities_from_disposal_tombstones pass unchanged (their rows are inserted after the handle's first commit, which records the namespace's rows as drained; see the report).
- added (runner): legacy_namespace_drains_50_cohorts_and_500_entries_per_commit, legacy_live_cohorts_survive_the_drain_and_leave_when_they_finish, inspection_lists_each_legacy_run_once_at_every_commit_of_the_drain, drained_legacy_runs_are_the_oldest_in_the_rolling_history, legacy_admin_receipts_leave_in_bounded_steps_dated_by_their_own_time, legacy_keyed_receipts_and_events_join_the_window_and_the_eviction_order, drain_bounds_come_from_the_stored_policy_and_must_be_positive, a_drain_continues_after_a_restart, gc_leaves_legacy_runs_to_the_drain, a_new_namespace_is_drained_by_its_first_commit, legacy_events_are_ordered_in_bounded_steps_and_never_all_rejudged_at_once, memory/record_storage_vendored_namespace_drains_while_new_runs_are_served; lib retention::window_tests::settle_cap_rearms_the_drain, a_drained_namespace_does_no_drain_work. (postgres): legacy_protected_namespace_drains_records_and_sql_tombstones, drained_tombstone_rows_cost_no_statement_per_commit, vendored_namespace_drains_on_postgres_while_new_runs_are_served, a_namespace_the_vendored_engine_wrote_on_postgres_drains (#[ignore], env-gated).

### Task 10 item A (receipt index rows keyed by sequence)

- no existing test rewritten; no assertion changed. The PostgreSQL index-query parity test (native_indexes_match_memory_and_inspection_uses_bounded_keyset_pages) passes unchanged because both backends now answer a receipt item's key with its sequence (20 zero-padded digits) instead of its position. Added: postgres lib indexes::projection_tests::receipt_rows_change_only_for_the_receipts_a_commit_removes_or_adds, history_tests::receipt_rows_are_deleted_by_sequence_through_an_index; postgres live_state a_commit_that_removes_a_cohort_writes_receipt_index_rows_for_that_cohort_only, receipt_index_rows_keyed_by_position_are_replaced_as_their_receipts_leave, rewriting_receipt_rows_over_rows_keyed_by_position_leaves_one_row_each.

### Task 10 item D (per-commit bounds are policy fields)

- no existing test rewritten. Added: per_commit_bounds_are_policy_fields_read_from_the_stored_policy, the_eviction_licence_reads_its_bound_from_the_policy.

### Task 10 items B/F (collect_garbage a thin wrapper; legacy disposal paths removed)

`crates/aq-playbooks-runner/tests/live_state.rs`

- memory_storage_allows_legacy_removals / record_storage_allows_legacy_removals (legacy_removals_are_allowed) -> memory/record_storage_legacy_records_license_no_removal (legacy_records_license_no_removal). Old: a run removed without history was allowed because the commit carried a legacy `run` disposal record, and an event removed with a legacy tombstone had its digest checked ("event disposal digest conflict"). New: the disposal-record licence is gone, so that removal is refused (REFUSE_RUN_REMOVAL); legacy tombstones and correlation fences may still always be removed, and an expired event leaves by its window. Reason: spec 0003 §6 / item B, no removal of a finished run without history; no code writes event tombstones, so the digest check had no writer.
- legacy_tombstones_beyond_max_tombstones_stop_neither_gc_nor_policy_changes -> max_tombstones_stops_neither_gc_nor_policy_changes_and_gc_writes_no_tombstone. Old: GC removed 3 expired events writing 3 tombstones. New: the GC commit's window drop forgets them, 0 tombstones; max_tombstones 1 still stops neither GC nor the policy change. Reason: GC writes no tombstone.
- a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains — collect_garbage(...).runs 0 -> 1. Reason: collect_garbage is an ordinary commit and takes the drain's next step (the last legacy run goes to history in it); before, GC skipped the drain's cohorts.
- gc_leaves_legacy_runs_to_the_drain — deleted. Reason: it asserted that GC removes 0 legacy runs during a drain (a guard against GC's run removal without history, which no longer exists); replaced by collect_garbage_on_a_legacy_namespace_advances_the_drain (GC moves them to history through the drain, all 120 listed) and collect_garbage_never_removes_a_finished_run_without_history.

`crates/aq-playbooks-runner/tests/semantics.rs`

- gc_tombstones_expired_event_identity_and_epoch_fence_compacts_safely -> an_expired_event_identity_is_judged_by_time_and_the_epoch_fence_refuses_old_epochs. Old: GC tombstoned the expired event and a redelivery was refused by the tombstone. New: the event leaves with no tombstone; a repeat carrying its producer time is Error::Stale and writes nothing; after the epoch is retired, the old epoch is refused (unchanged). The artifact is now activated (install + activate) so GC's artifact phase keeps its event contract (the old test's `is_err()` also matched "unknown event contract").

`crates/aq-playbooks-runner/tests/review_regressions.rs`

- expired_replay_tombstones_and_retired_epochs_refuse_old_identities -> expired_replay_identities_are_judged_by_time_and_retired_epochs_refuse_old_ones. Old: 1 tombstone after GC, redelivery refused. New: no tombstone; with its producer time stale (Error::Stale), without one new; the retired-epoch and new-epoch assertions unchanged. (Brief: "tombstones refuse old identities" -> "expired identity without producer time is new; with stale producer time refused".)

`crates/aq-playbooks-postgres/tests/protected_indexes.rs`

- event_gc_replay_tombstones_survive_restart_and_explicit_new_epoch_has_new_identity -> an_expired_event_identity_is_judged_by_time_after_restart_and_a_new_epoch_has_a_new_identity. Old: each GC wrote one `event` disposal_tombstones row that refused the identity after a restart. New: 0 rows and no tombstone; after the restart a repeat with its producer time is stale; epoch retirement and the new epoch unchanged.
- forged_disposal_cannot_erase_unresolved_children_or_external_effects -> a_disposal_record_licenses_no_removal_of_unresolved_children_or_external_effects. Old: refused (any error). New: refused with exactly REFUSE_RUN_REMOVAL — a disposal record licenses nothing. (Brief: "forged disposal" -> "removal without a history record is refused".)

`crates/aq-playbooks-postgres/tests/live_state.rs`

- a_legacy_row_written_after_the_drain_is_deleted_by_the_next_commit — deleted. Reason: it tested that a commit writing legacy tombstone rows (legacy GC's event removal) re-arms the per-handle row drain; no code writes such rows any more, and the re-arm (TombstoneRows::wrote) is removed with them.
- legacy_tombstone_rows_beyond_max_tombstones_refuse_no_commit_and_are_not_scanned — last step. Old: the commit that moved the GC clock deleted the 1,000 legacy `run` rows through the cutoff delete. New: the cutoff delete takes artifact rows only; the run rows are left to the drain — the handle that has seen the namespace drained does not look again, and a new handle deletes them 500 per commit (1,000 -> 500 -> 0).

`crates/aq-playbooks-cli/tests/admin_cli.rs` (lines 264-270)

- protected_admin_lifecycle_control_recovery_rotation_and_paged_inspection — added assertions: `gc --help` says "Not needed"; the gc report has `admissions` 0. The `runs` 0 assertion is unchanged.
- checked, unchanged (they pass and still assert their point under the live-state engine): semantics gc_preserves_bound_transitive_closure_then_collects_terminal_cohorts_in_pages (artifact closure), review_regressions completed_cohort_gc_removes_ready_timer_and_dependency_index_residue, garbage_collection_bounds_preserved_candidate_examination (now artifacts only), garbage_collection_preserves_transitively_bound_artifacts_without_runs, retired_producer_epochs_need_no_new_tombstones_for_late_gc, keyed_admission gc_and_direct_storage_cas_cannot_forget_or_rebind_a_key. (producer_epoch_compaction_… and pruned_general_wait_… were rewritten in Task 6.)
- added: runner collect_garbage_is_optional_and_reports_the_bounded_drop (the plan's test), collect_garbage_writes_no_tombstone_fence_or_disposal, collect_garbage_on_a_legacy_namespace_advances_the_drain, collect_garbage_never_removes_a_finished_run_without_history; postgres collect_garbage_writes_no_disposal_tombstone_row_on_postgres.

### Task 10 item C (a lost handle's hold lapses after the command deadline plus the window)

`crates/aq-playbooks-runner/tests/semantics.rs`

- gc_preserves_possible_async_completion_while_lost_admission_handle_is_unknown — times only: collect_garbage at 4000 -> 1050 and recover/drive at 4001 -> 1051. Reason (ruling C): the hold now lapses at the operation's command deadline (100) plus the completion window (1,000) = 1,100; at 4,000 it had lapsed and the expired completion is dropped. 1,050 is past the event's own window (1,001) and inside the hold, so the test still asserts what it asserted: the completion survives GC and the found handle takes it (outcome ok). Kept, as the brief asks ("keep the async-handle preservation assertions").
- added: runner a_lost_handle_hold_lapses_after_the_command_deadline_plus_the_window, a_handle_found_before_the_hold_lapses_takes_its_held_completion, a_handle_found_after_the_hold_lapsed_resolves_by_the_deadline_and_recovery_path.

### Task 10 fix round 1

- history_tests::receipt_rows_are_deleted_by_sequence_through_an_index — now runs in a database of its own (CREATE DATABASE … / DROP … WITH (FORCE)); assertions unchanged. Reason: the planner's choice depends on what other tests left in the shared tables; in the full suite the unfenced plan was a hash join over a seq scan, alone it was the index, so the test could not tell the fence mutant apart.
- per_commit_bounds_are_policy_fields_read_from_the_stored_policy — also covers settle_cohorts_per_commit (default 50, decoded default, 0 refused).
- added: postgres live_state opening_the_storage_analyzes_engine_tables_that_have_no_statistics, a_role_that_cannot_analyze_still_opens; runner lib retention::window_tests::the_settle_bound_comes_from_the_policy; runner live_state a_lapsing_hold_rejudges_its_events_in_bounded_batches, a_second_lapse_rejudges_from_the_earliest_change.
