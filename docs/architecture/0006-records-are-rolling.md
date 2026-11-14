# Records are rolling: no caps, old records are deleted

**Status:** Required platform principle (owner, 2026-11-13). It applies to
every AQ repository. Almost none of the target state below is implemented;
each spec that follows it says so in its own status line.

**Date:** 2026-11-13

## 1. The rule

The owner, 2026-11-13:

> "the fundamental issue I have is a record growing linearly with each run"

> "How about we just don't add 'cleanup' and we never create the problem in
> the first place."

> "Doesn't replay protection just need to know the last one or maybe two
> runs? And history runs we should just keep a rolling log of some number of
> them. Nothing should have cap that blocks additional runs. The expectation
> with playbooks is that they trigger often and over time we will have
> infinite runs."

> "Just remove all 'limits' and just enforce deleting old records. This
> should be how ALL SYSTEMS that log in agent queue are designed."

Stated plainly:

1. **Rolling sets.** A store that accumulates per-item records (runs,
   attempts, events, messages, receipts, operation journals, history,
   transcripts, heartbeats, audit rows) keeps a rolling set: the most recent
   N, the last T, or both. It deletes older records itself, as part of normal
   operation. No operator action, and no separate cleanup that someone must
   remember to call.
2. **No caps.** No store refuses, holds or blocks new work because it is full
   or because a lifetime counter was reached. There are no caps on
   accumulated records.
3. **Finished means deleted.** A record is deleted when the thing it
   describes is finished, unless it belongs to one of the rolling sets above.
4. **Repeat protection rolls too.** Idempotency keys, operation ids and
   replay receipts are a rolling set. A repeat is recognised while its key is
   among the most recent N, or inside the window. After that the key is
   forgotten. A very late repeat is handled by the store's documented rule:
   refused as expired where the request carries its own time, otherwise
   accepted as new. A store does not keep every key forever to catch every
   possible duplicate.

A store that follows this rule has a size that does not depend on how many
runs have ever happened. Over any length of time the number of runs is
unbounded.

## 2. What the rule does not cover

These stay, and a spec that keeps one says why:

- **Records that are the product:** tasks, projects, settings (the current
  values), installed artifacts, and the knowledge graph's own node history.
- **Limits on the size of one input:** payload bytes, field lengths, items
  per request, a per-response cap.
- **Controls on work happening at this moment:** concurrency, rates (for
  example offers per node per hour), the owner's swarm matrix, worker slots,
  live-row backstops that count only work in progress.
- **Budgets the owner set on purpose:** per-task retry, time and cost
  budgets, sandbox-minute budgets, provider quota. Deleting old attempts must
  keep the counts these budgets read. The store that deletes an attempt first
  folds what the budget needs into a summary that lives as long as the task.
- **Bounds on deletion work:** how many rows one pass or one commit deletes.

Physical safety stops (disk above its high-water mark, a full filesystem of
unsaved work) are not record caps either. They protect the machine, and they
name what holds the space.

## 3. How a store follows it

- Deletion runs where records are written: in the same transaction (keep the
  newest N on insert), in the commit that finishes the work, or in a job the
  service already runs (the server's ten-minute DEP6 tick,
  [row retention](../operations/row-retention.md)). It deletes a bounded
  batch per pass and drains a backlog over later passes.
- A lifetime counter is removed, or replaced by a count of live rows. A
  trigger that forbids a counter from going down is removed.
- A "cannot be deleted" guard on a log table is relaxed for the deleting
  job only.
- A reader that used a deleted record says what it does instead: shows the
  record as no longer kept, reads a summary, or treats a late repeat by the
  store's rule.
- Every N and T is a setting with a default. The defaults below are
  proposals.

## 4. Stores and their target state

Every number in this table is a **proposed default the owner can change**,
except where it says owner. Where a store already had a cap, the cap's value
is proposed as the size of its rolling set, so nothing that fits today stops
fitting. "Pending" means not built.

| Store | What it keeps | How old records leave |
| --- | --- | --- |
| Supervisor conversations, global and per project (aq-server) | The most recent 4,096 messages and 64 MiB per conversation | Oldest messages deleted on insert. The "cannot decrease" trigger goes. Pending |
| Central supervisor actor operations (aq-server) | Unsettled operations, and the most recent 4,096 settled ones per actor | Oldest settled deleted on insert. Pending |
| Notifications: messages, deliveries, operations (aq-server) | Open episodes, and settled items for 30 days, at most the most recent 4,096 per destination | DEP6 tick. Lifetime counters removed. Pending |
| Dashboard task-creation journal `human_graph_operations` and dashboard operation journals (aq-server) | Unsettled operations, and settled ones for 7 days | DEP6 tick; `dashboard_actions` `retire()` is called by it. A forgotten id is accepted as new. Pending |
| Server playbook bridge `playbook_namespaces` counters, host operations, findings (aq-server) | What aq-playbooks spec 0003 keeps: live work, the most recent `replay_keep` keys (256), the most recent `history_keep` runs (10,000) | Counters removed; host operations and findings leave with their run's history. Pending |
| Review budgets (aq-server) | Open reviews; decided reviews leave with their run (spec 0008 phase B) | Lifetime operation counter removed. Per-task count: see §6. Pending |
| Result media (aq-server, [spec](../specs/result-media-png.md)) | Bytes for 7 days (existing); intent rows while their report exists | Automatic sweep on the DEP6 tick. Lifetime intent counts removed; at the byte budget the oldest bytes leave first. Pending |
| Profile catalogs (aq-server, [operations](../operations/profile-catalogs.md)) | The active catalog, any catalog a live attempt pins, and the most recent 128 others; activation receipts the most recent 4,096 | Oldest deleted on activation. Pending |
| Settings save histories: profiles, structure, internal work, swarm providers, supervisor control (aq-server) | Current value (product), and the most recent 100 revisions and receipts per kind | Oldest deleted on save. Pending |
| Terminal registry (aq-server) | Live sessions; ended ones for 7 days (DEP6); the most recent 1,024 operations per terminal | DEP6 tick and on insert. Lifetime counts removed. Partly built (DEP6) |
| Observation ledger (aq-server) | Events, facts, packets for 14 days after the task settles (DEP6); live sources | DEP6 tick; counters count live rows. Source bindings, revocations and transport receipts leave 14 days after their task settles. Partly built |
| Worker heartbeat log `profiled_presence_operations` (aq-server) | Reports of the last 24 hours, and any a live run still needs | DEP6 tick. Pending |
| Sealed attempt records (aq-server, [spec 0008](../specs/0008-sealed-history-retention.md)) | Full records while the run is live or its task open, and 7 days after the task closes (**owner**, 2026-11-07); a summary of each of the most recent 10,000 settled attempts per project; one budget summary per task | Spec 0008 phase B sweeper. Pending |
| Sealed native-path receipts B1–B8 (aq-server, spec 0008 §5.2) | Rolling per node (for example registration history: the most recent 16 boots per node) or deleted with their run | Spec 0008 phase B. Pending |
| `command_audit` (aq-server) | The last 30 days | DEP6 tick. Pending |
| Internal-work runs and transcripts (aq-server, [spec 0012](../specs/0012-internal-work.md)) | The most recent 10,000 runs and none older than 30 days; a transcript keeps its newest items | DEP6 tick. Pending |
| Routing compiles and authoring runs (aq-server, specs [0011](../specs/0011-routing-policy.md), [0013](../specs/0013-playbook-authoring.md)) | The active and previous playbook (product), and the most recent 20 runs per source | Oldest deleted when a run is written. Pending |
| Route history `task_routes` (aq-server, spec 0011) | The current route (in the graph), and the most recent 16 routes per task, until 30 days after the task settles | On insert and DEP6 tick. Pending |
| Agent graph operation intents (aq-server, [graph mutations](../operations/graph-mutations.md)) | Unresolved intents, and the most recent 4,096 settled ones per attempt | Oldest settled deleted on insert; deleted with the run (spec 0008). Pending |
| Operation receipts: comments, mailbox, delivery, central host launches, manual sessions, worker settings/login/diagnostic operations, handoffs, hibernation, waits (aq-server) | Unsettled operations, and settled ones for 7 days | DEP6 tick. Pending |
| Manual-agent transcripts (aq-server, [spec 0009](../specs/0009-agents-terminal-workspace.md)) | The newest 64 MiB per session; sessions ended within the last 90 days | Oldest chunks deleted as new ones arrive; DEP6 tick for old sessions. Pending |
| Project registry revisions and receipts (aq-server, [spec 0003](../specs/0003-project-execution-bindings.md)) | Current projects (product); the most recent 100 revisions per project; receipts for 7 days | On save and DEP6 tick. Pending |
| Intelligence receipts and decision metadata (aq-server, [spec 0004](../specs/0004-intelligence-selection.md)) | The most recent 10,000 owner receipts, 4,096 machine receipts per node, 1,024 credential definition revisions; decisions with their run | On insert; spec 0008 for decisions. Pending |
| DEP6 tables (aq-server, [row retention](../operations/row-retention.md)) | As listed there | DEP6 tick. Built for completed tasks; cancelled and retired tasks pending |
| Worker per-attempt archive directories (aq-worker) | Directories holding unretained work; the most recent 20 others per slot, none older than 14 days | Worker service on its existing reconcile pass. Pending |
| Preserved and kept sandboxes (aq-worker, [0005 SBX22](0005-sandboxed-execution-and-deployment-topology.md)) | The most recent `preserve.max_count` per node, none older than `preserve.max_age` | Oldest destroyed when a new one is preserved, after a notice. Pending; see §6 |
| Worker verbose log spool (aq-worker) | The newest entries within the spool size | Oldest deleted first, with a gap marker. Pending |
| aq-git operation journal (aq-git) | Unresolved operations, and terminal records of the last 30 days, at most the most recent 50,000 per state root | aq-git deletes them in its own admission path. Pending |
| aq-git retired workspaces, released handles, old bindings (aq-git) | The most recent 16 binding versions per repository; retired and released records with the journal | With the journal. Pending |
| aq-git unreferenced objects (aq-git) | Objects some retention ref or workspace still needs | Automatic prune after release and a grace period. Pending |
| aq-graph idempotency receipts (aq-graph) | Receipts of the last 7 days | aq-graph deletes them on its own schedule. A forgotten key is accepted as new. Pending |
| aq-graph request logs and diagnostics (aq-graph) | Three files of 10 MB (existing Docker rotation) | Rotation. Built |
| Playbook engine records (aq-playbooks, spec 0003) | Live work; the most recent `replay_keep` keys (256); the most recent `history_keep` finished runs (10,000) | In the commit. In progress |

Records that are the product and stay: graph tasks and knowledge with their
node history, projects, current settings, installed playbook and catalog
artifacts in use, aq-git source that a live retention handle names.

## 5. What readers do after a record is gone

- **A late repeat of an operation id.** Handled by the store's rule (§1,
  point 4). A store whose requests carry their own time refuses it as
  expired. Otherwise it is accepted as new, and the store's own preconditions
  (expected version, expected generation, expected old OID) refuse most
  stale repeats. Callers retry with the same id only within the store's
  window. An uncertain operation older than the window is resolved by
  reading the current state, not by replaying.
- **Budgets.** A retry, time, sandbox-minute or cost budget reads a summary
  that the deleting job writes first and that lives as long as the task.
  Monthly spend reads month totals the deleting job keeps for the current and
  previous month.
- **History pages.** A page that lists runs, sessions or results shows what
  is kept and says that older ones are no longer kept. It never fails because
  an old record is gone.

## 6. Questions for the owner

1. **Proposed defaults.** Every number in §4 marked proposed.
2. **Preserved sandboxes hold unsaved work.** Under this rule a node at
   `preserve.max_count` destroys its oldest preserved sandbox instead of
   refusing new work, and `preserve.max_age` destroys rather than only
   re-notifying. That deletes work nobody copied back. Is that what you want,
   or is a preserved sandbox work in progress (not covered)?
3. **Per-task review limit (256 per task lineage).** Is it a budget you want
   (keep), or a cap (remove)?
4. **Phase A live caps (spec 0008).** They count only live rows, so this
   document treats them as controls on work happening now and keeps them.
   Keep, or remove as well?
5. **Duplicate risk after the window.** Dashboard, comment and graph
   requests carry no request time, so a repeat after its window is accepted
   as new and could create a second task or comment. Is a 7-day window
   enough, or should these requests carry their time so a late repeat is
   refused?
6. **Profile catalog identity history.** It records every definition
   identity and version ever published, so a version cannot be reused with
   other content. It grows with edits, not runs, and has a 4 MiB bound that
   refuses activation. Is it product (keep the bound as a limit), or a rolling
   set?
7. **aq-git forgotten operation ids.** A forgotten id is accepted as new and
   then checked against its preconditions. Should aq-git instead require a
   request time so it can refuse a late repeat as `operation_expired`?
