# 0003 — The engine's record holds only work in progress

Status: approved by the owner on 2026-11-13 (approval given in advance; the
design calls below are marked so they can be checked).

## Why

The engine keeps one state record per namespace and saves the whole record on
every step. Finished runs stay in that record. So the record, and the cost of
every step, grow with every run that has ever finished.

Measured on 2026-11-13 (`aq-server` worktree,
`.superpowers/sdd/2026-11-12-routing-policy/engine-growth-report.md`):

| Setup | Added per finished run | Step time, run 1 → run 1000 |
| --- | ---: | --- |
| Protected storage, as aq-server runs today | 18.5 KB | 17 ms → 676 ms |
| Same, with `collect_garbage` after every run | 815 B, never removed | 19 ms → 50 ms |
| Per-item storage (writer format 3), no cleanup | 13.1 KB | flat at 15 ms |

No setup levels off. `collect_garbage` itself costs in proportion to the whole
record, and aq-server never calls it. In an aq-server scale test the record
reached 7.8 MB after 206 runs and a step took 2.8 s.

The owner's requirement: do not add cleanup. Do not create the problem. When
a run finishes, save over the record without it.

## Goals

1. The state record of a namespace holds work in progress and a time-limited
   window of repeat protection. Its size does not depend on how many runs have
   finished.
2. No host has to call anything to keep it that way. `collect_garbage` is not
   needed for correct or bounded operation.
3. Finished runs stay inspectable, from rows that are written once and never
   rewritten.
4. Works with protected storage (writer format 2) as it is. No namespace
   changes storage format and no encryption setting changes.

## Non-goals

- Bringing per-item storage (format 3) to protected namespaces.
- Changing playbook semantics, the catalog, or the public shape of inspection
  results.
- Removing encryption. It is not the cause of the growth.

## Design

### 1. A finished root leaves the record in the commit that finishes it

A *cohort* is a root run and every run that belongs to it (the engine's
`root_members`). A cohort is *settled* when every run in it is terminal and
none of its operations is unsettled. This is the condition `gc` uses today,
without the age test.

In the same transition that settles a cohort, the engine removes from the
state everything that belongs to it: its runs, intents, recovery requests,
waits, receipts, budget and index entries. Nothing waits for a clock.

Removing the cohort as a whole is deliberate. A parent that is waiting on a
child, a join, and a root deadline all read a finished run's result
(report, readers 1–4). All of those readers are inside the cohort, so none of
them can outlive the data they read. A cohort with an operation still
unsettled (for example a cancelled operation whose late result has not
arrived) stays until it settles, as today.

Commands addressed to a run that has left the record (`advance`,
`request_cancel`, a late wake) keep today's behaviour for a collected run.

### 2. History rows

The removed cohort is handed to storage in the same commit as *history*:
one row per finished run, holding exactly what inspection returns for a run
today (the public run projection with its effects and receipts), plus the
namespace, run id, root id and finish time for lookup.

- A history row is inserted once and never updated.
- History holds public projections only. Protected inputs and results are not
  written to it (spec 0001 §14 already separates them), so history rows are
  not encrypted.
- Inspection answers from the live state and the history rows together. Page
  order and filters are unchanged for callers.
- The storage trait gains the history write as part of the commit and a
  history read. Backends: PostgreSQL (a new table, new migration), the
  in-memory reference backend, and any other backend in the workspace.
- History is kept until the host deletes it. The engine offers one call that
  deletes history older than a given time. That is an ordinary delete of
  rows by an indexed column; nothing else depends on it. aq-server keeps
  history (design call: it is the server's run history, like finished tasks).

### 3. Repeat protection is a time window, enforced on every commit

Four things are kept after a run today and are never removed
(report, "What GC leaves behind"): keyed admission receipts, event replay
tombstones, expired-correlation fences, and SQL disposal tombstones. Events
themselves are also kept until their replay horizon.

All of them become window entries with an expiry time fixed when they are
written:

| Entry | Expires |
| --- | --- |
| Admitted event (kept so a later wait can match it, and to refuse a duplicate) | admission + `max(policy.event_replay_ms, contract.retention_ms)`, as today |
| Keyed admission receipt | acceptance + `policy.admission_replay_ms` (new; default 7 days) |
| Event tombstone and correlation fence | no longer written; see below |
| SQL disposal tombstones | no longer written for runs, intents or events removed under this spec |

Every commit drops the window entries whose expiry has passed, up to a fixed
number per commit (design call: 64), oldest first. This is part of the
transition, not a separate call, and its cost depends on the number dropped,
not on the record. Entries are kept in expiry order so finding them does not
scan the record.

After an entry expires, the engine no longer remembers that key. A request
that repeats an expired key is handled by time, not by a remembered key:

- Events and keyed admissions may carry the producer's time for the request.
  A request whose producer time is older than the window is refused as
  stale. This is spec 0001 §14's "beyond the supported horizon, stale
  requests are rejected", done without keeping a key per finished run.
- A request with no producer time and an expired key is accepted as new.
  Design call: this is the cost of keeping nothing. Hosts that need the
  refusal send the producer time. The implementation records in
  `docs/implementation/` which request types carry it.

Producer epochs stay. Retiring an epoch still refuses everything from older
epochs; it is no longer needed to free storage.

Existing artifact retention is unchanged, except that it no longer has
finished runs to scan: an artifact is referenced by live runs and by the
catalog rules it has today.

### 4. Quotas count live work

`max_runs`, `max_receipts` and `max_state_bytes` count what is in the record.
After this change that is work in progress plus the window. `max_tombstones`
is unused and is accepted but ignored in a stored policy.

### 5. Existing namespaces

A namespace written by an older engine holds settled cohorts and old
tombstones. The first commits after the upgrade move them out, a bounded
number per commit (design call: 50 cohorts and 500 tombstone or fence
entries), as history rows and dropped entries. No offline migration, no
format change. Until the backlog is gone the namespace behaves correctly and
shrinks with every commit. Stored states from before this change must still
decode.

### 6. `collect_garbage`

Kept so existing callers compile. It runs the same bounded drop as a commit
and reports what it removed. Nothing requires calling it.

### 7. Format 3

Format 3 gets the same behaviour through the shared transition code. Removing
a cohort is a "heavy" commit there today. That is acceptable because the
record is now small; making it a light commit is left to the builder if it is
cheap.

## aq-server

- Update the vendored engine to the version with this change and adapt its
  call sites.
- Set `event_replay_ms` and `admission_replay_ms` to 10 minutes for the
  server's namespace (design call). The server delivers events from its own
  outbox and retries within seconds; a contract that needs a longer window
  declares `retention_ms`.
- The playbook runs list and run timeline keep working through inspection.
- The server keeps using protected storage.

## Acceptance

Run with the benchmark from the measurement
(`crates/aq-playbooks-postgres/examples/engine_growth.rs`, brought onto this
branch), protected format 2, never calling `collect_garbage`:

1. **Steady rate.** 10,000 runs, the clock advancing 10 s per run, 10-minute
   windows. Record size and median step time at run 10,000 are within 1.2× of
   their values at run 200.
2. **Burst.** 1,000 runs with the clock not advancing. The record grows by no
   more than 2 KB per run, and nothing but window entries is left per run.
   After the clock passes the window and 20 more runs complete, the record is
   back within 1.2× of its size at run 1.
3. **History.** After (1), inspection lists all 10,000 runs and returns the
   same projection for a finished run as it did before the run was removed.
4. **Upgrade.** A namespace holding 1,000 finished runs written by the
   current engine drains to the steady size while new runs are served, and
   every old run appears in history.
5. **aq-server.** The 500-run routing scale test
   (`docs/implementation/0011-R3-scale.md` in aq-server) passes its limit.
6. The existing engine test suites pass. Tests that asserted finished runs
   remain in the state are rewritten to assert history instead; each such
   change is listed in the implementation notes.

## Left to the builder

- The shape of the history table and its indexes.
- How window entries are ordered and stored inside the record.
- Whether cohort removal can be a light commit on format 3.
- The exact error text for a stale request.

## Rulings after the builder check (2026-11-13)

The plan (`docs/superpowers/plans/2026-11-13-live-state-only.md`, "Spec
check") tested this spec against the code. Where the two differ, these
rulings apply:

- **Waits and expired events.** Correlation fences are not kept. A wait
  registered after its event's window has passed times out; it is no longer
  refused at registration. This is the one playbook-visible change.
- **Late results.** A cancelled operation already counts as settled, so its
  cohort leaves at once and a late result for it gets "not found". The
  example in §1 is wrong on this point.
- **Producer time.** `occurred_at_ms` is business time and is not used. The
  stale check uses an optional request time passed to new engine methods and
  not included in any digest.
- **Deferral.** An expired event that a live wait, a queued destination or an
  unknown completion handle still needs, and an expired keyed receipt whose
  run is still live, stay until that need ends.
- **History rows.** One row per inspection item, not one per run. Receipts
  owned by no run (`admin`, `event:<key>`) also go to history.
- **Acceptance 2, last clause.** Replaced by: drain with the clock frozen,
  move the clock past the window once more, complete one more run, then
  compare the record with its size at run 1 (within 1.2×). The literal
  figure for the original wording is reported too.
- **Burst bound.** The 2 KB per run bound stands. If the measurement
  exceeds it, report the figure; do not change the bound.
- **Version.** The engine crates go to 0.2.0.

## Owner direction, 2026-11-13 (later): rolling bounds, and nothing finished may block new runs

> "Doesn't replay protection just need to know the last one or maybe two runs?
> And history runs we should just keep a rolling log of some number of them.
> Nothing should have a cap that blocks additional runs. The expectation with
> playbooks is that they trigger often and over time we will have infinite
> runs."

This changes §3 and §4:

1. **Repeat protection is a rolling set, bounded by count as well as by
   time.** The engine remembers the most recent `policy.replay_keep` event and
   request keys (design call: default 256), and forgets a key when it is older
   than its window *or* when newer keys push it out, whichever comes first.
   Forgetting never waits for a clock and never refuses anything. Two is too
   few as a single global number: when many tasks become ready together their
   events arrive together, and a re-send of the first must still be recognised
   after the others. An entry that work in progress still needs (a live wait, a
   queued destination, an unknown completion handle, a live run's keyed
   receipt) is not pushed out; it leaves when that need ends.
2. **History is a rolling log.** The engine keeps the most recent
   `policy.history_keep` finished runs per namespace (design call: default
   10,000) and deletes older ones as new ones are written, a bounded number
   per commit. Nothing reads history except inspection.
3. **Nothing that is finished or merely remembered can refuse new work.**
   Remembered keys and history never count toward any limit. The only limits
   that can make a new run wait are on work that is running at this moment
   (`max_runs`, and the receipts and bytes of live runs). Hitting one does not
   lose the request: the engine answers "busy" and the host delivers it again.
   Over any length of time the number of runs is unbounded.

## Owner direction, 2026-11-13 (final): no limits

> "Just remove all 'limits' and just enforce deleting old records. This should
> be how ALL SYSTEMS that log in agent queue are designed."

There are no namespace limits. `max_runs`, `max_receipts`, `max_state_bytes`
and `max_tombstones` are accepted in a stored policy and ignored; nothing is
ever refused because a namespace holds too much. The record is bounded only by
deletion: finished work is deleted when it finishes, remembered keys are the
most recent `replay_keep`, and history is the most recent `history_keep` runs.
Point 3 of the previous section is replaced by this. What stays is not a
namespace limit: the limits a playbook declares for itself, input size checks,
and the bounded amount of deletion work done per commit.
