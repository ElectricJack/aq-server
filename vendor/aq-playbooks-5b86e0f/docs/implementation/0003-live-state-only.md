# 0003 — Live state only: implementation notes

Spec: `docs/specs/0003-live-state-only.md`. The main notes are written at
the end of the plan (Task 12). This file starts with the request-time rule
(spec §3, ruling "Producer time").

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

