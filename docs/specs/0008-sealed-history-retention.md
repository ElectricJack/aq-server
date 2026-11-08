# 0008: Sealed-history retention (live-only caps, rate limits, deletion after the task closes)

**Status:** Draft for the owner's review (2026-11-08). The retention rule in §1 is the owner's decision of 2026-11-07. Everything else is a proposal.
**Repositories:** aq-server; aq-dashboard for the contract re-pin and the Settings field.
**Unblocks:** spec 0007 phase 1 live acceptance, which stopped on a history cap (§1.1).

Decisions are marked **(owner)** when the owner made them and **(author's call)** when the author of this spec filled a gap. The owner should check the second kind while reading.

## 1. Intent

AQ v2 keeps a "sealed" record of every admission and launch in Postgres: what was selected, which bytes went into the agent's context, which source was exported, which node was granted, what was proved at the stop. These rows are append-only and protected by delete guards. AQ-08 states the rule (`docs/implementation/AQ-08-authority-bridge.md:195-197`): "Live/referenced rows are pinned; expiry never deletes replay/stop proof. History growth refuses new work until an explicitly qualified maintenance policy exists." To keep that growth finite, several tables carry hard caps on the row count over the table's whole life. Nothing ever frees them, so a long-running swarm stops at the first cap it reaches.

The owner's rule (2026-11-07) **(owner)**:

- full sealed records are kept while the run is live or its task is open;
- full records are deleted 7 days after the run has settled **and** its task is closed. The 7 days can be changed in Settings;
- a summary of about 1 KB per run is kept forever;
- caps apply only to live rows, plus a rate limit.

In the owner's words: "I'm just not sure we need to retain them forever. I don't really understand why we need the records or when we would use them."

What the full records are for, in short: while a run is live they are its authority. Every accept, grant, renewal, stop and settlement re-reads them to prove the run is the one that was offered, which is why they are pinned. After the run settles, the readers that remain are the result and review pages, the session pages, replacement history if the task is reopened, and spend accounting (§8.2). None of those needs the full bytes once the task has been closed for a week; a summary row serves them. This spec is the "explicitly qualified maintenance policy" that AQ-08 asks for.

### 1.1 What stopped spec 0007 phase 1

Live acceptance of 0007 phase 1 on aq-ms1 offered 33 attempts over the stack's life, all in one project. The 34th failed when it inserted an admission artifact. The `admission_artifact_insert_guard` trigger (`crates/aq-server/migrations/0025_native_source_observations.sql:9-10`) raised `admission artifact quota`, because the project already held 512 rows. `artifacts::retain` (`crates/aq-server/src/profiled_execution/artifacts.rs:262-264`) turned that into the 409 "inline artifact owner unavailable or inconsistent". The poll failed with a redacted `Failure{code: Conflict}` (`profiled_execution/control_api.rs:22-56`). Every later poll did the same, while the Swarm page still showed the machine `online` with no waiting reason.

The earlier analysis (S16, `.superpowers/sdd/2026-11-07-swarm-p1-workers-as-services/server-lane-report.md`) expected the first stop at about 256 attempts. It did not list the admission-artifact cap, the phase-terminal cap of 16 per node, the preparation byte budget or the presence-report counter (§5).

## 2. Goals

1. A swarm that runs for months stops on no history cap. Caps count only live rows, and a rate limit bounds growth.
2. When a cap or the rate limit holds work back, the Swarm page and `GET /api/v1/swarm` say which one and why. A worker never loops on a silent 409.
3. Full records of a run are deleted once the run has settled and its task has been closed for longer than the retention period: 7 days by default, set in Settings.
4. A summary of about 1 KB per run is kept forever, so task history, session lists, results, cost and replacement history still work after deletion.
5. Deletion runs in the background, in bounded batches, and never removes a row that something live still references.

## 3. Non-goals

- **Shrinking Postgres files.** Deleted rows are reclaimed by autovacuum; nothing runs `VACUUM FULL` (as in `docs/operations/row-retention.md`).
- **Retention of Git sources.** aq-git and source GC (`source_retention/gc.rs`) keep their own rules.
- **Observation, mail, outbox and terminal retention** that DEP6 row retention already covers. The exception is observation rows of a run being deleted, which go with the run (§8.4).
- **Graph task history.** Task comments and task state belong to Graph and are not touched.
- **Archiving to another store.** Deleted records are gone. The summary is the archive **(author's call)**.

## 4. Anti-goals

These would count as failure even if everything else works:

- **A silent stop.** Work held back by a cap with no visible reason, as on 2026-11-07.
- **Deleting authority that is still in use.** Removing a row of a live run, of a run whose task is open, or a shared row that a live run references.
- **A page that breaks after deletion.** A result, session or task-history page that returns an error because the sealed rows of an old run are gone.
- **Raising the caps instead.** Bigger lifetime numbers only move the stop (S16 option C).

## 5. Inventory of history caps

Measured on aq-ms1 on 2026-11-08 at state format 102, with read-only queries. The stack had 33 offered attempts (all `profiled_v1`, one project `e0000000-…-0002`, 12 nodes, 30 phase grants), 26 started, 23 stopped, 22 results. "Rows per attempt" is measured as rows divided by offered attempts. "Attempts to exhaustion" assumes one project and, for per-node caps, one long-lived node, which is the always-on service case of spec 0007.

### 5.1 Lifetime caps on the native swarm path (converted in phase A)

| # | Table (what is counted) | Cap | Scope | Enforced at | Rows per offered attempt (measured) | Attempts to exhaustion |
|---|---|---|---|---|---|---|
| A1 | `profiled_phase_terminal_capacity` (all rows) | 16 rows, 16 MiB | per node | trigger `profiled_phase_terminal_capacity_guard`, 0033:15-26 (DELETE refused at :18) | 1 per phase grant (30 / 33 = 0.91) | **about 17 on one node**; the busiest ms1 node holds 11. Global 256 rows → about 280 |
| A2 | `admission_artifacts` (all rows) | 512 rows, 64 MiB | per project | trigger `admission_artifact_insert_guard`, 0025:2-35 (first 0020:16); insert `artifacts.rs:262-264` | 14.8 new rows (488 / 33, plus 24 catalog rows); IDs dedupe only within a UTC day (`producer/artifacts.rs:73-76`, `build.rs:270-274`) | **33 (reached)**. Global 4096 rows → about 275 |
| A3 | Preparation retained bytes: leases, legacy execution receipts, material receipts, v2 proposals and accepts, phase grants | 4 MiB | per node | Rust `preparation.rs:480-491` (`retained_quota_locked`), checked at `preparation.rs:431-441`, `accepted_preparation.rs:52-85`, `initial_grant.rs:603-615`, `phase_execution/owner_core.rs:326-336` | 77 KB (846,972 B / 11 on node 188de335) | **about 54 per node**. Global 64 MiB → about 870 |
| A4 | `profiled_presence_operations` (counter `profiled_presence_node_storage.operations`) | 4096 operations, 16 MiB | per node | trigger `profiled_presence_operation_guard`, 0029:70-98, ERRCODE 54000 | not per attempt: one row per inventory report. 87 an hour on an active node (one every 41 s), 333 an hour at the measured peak | **about 47 h of running per node** (12 h at the peak rate). Global 65,536 |
| A5 | `intelligence_decisions` (all rows) | 1024 rows, 64 MiB | global | Rust only: `intelligence/resolver.rs:340-351` | 3.15 (104 / 33: 33 linked, 71 never linked) | about 325 |
| A6 | `admission_material_leases` (all rows, any state) | 256 rows | per node | Rust `preparation.rs:431-432`, `accepted_preparation.rs:72-73` | 1 | 256 per node; global 4096 |
| A7 | `profiled_v2_proposals` (all rows) | 256 rows | per node | Rust `accepted_preparation.rs:65-72` | 1 | 256 per node; global 4096 |
| A8 | `profiled_launch_registrations` (all rows) | 256 rows, 16 KiB | per node | trigger `profiled_launch_registration_guard`, 0028:60-74; Rust `closure/history.rs:151-156` | 1 | 256 per node; global 4096 |
| A9 | `profiled_source_exports` (all rows; metadata bytes) | 256 rows, 64 MiB metadata | per project | trigger `profiled_source_export_guard`, 0022:16-66 (DELETE refused at :19); Rust `source_exports.rs:340-355` | 0.88 (29 / 33); retries of one generation reuse the row (`workflow.rs:254-273`) | about 290 per project; global 4096 |
| A10 | Staging bytes: `sum(charged_bytes)` over **all** source exports, plus unreleased transfers and source bases | 4 GiB (`source_retention.rs:75`, configurable `max_staging_bytes`) | global | Rust `source_exports.rs:331-339`, `source_retention.rs:398-405` | 0.88 × bundle size. ms1's test repository: 4.2 KB per export | about 1,000,000 on ms1; for a 50 MiB bundle, **about 93** |

### 5.2 Lifetime caps not reached on the measured path (converted in phase B, author's call)

These are lifetime too, but aq-ms1 wrote zero rows to them in 33 attempts, or they grow per worker restart rather than per attempt. A 650-attempt soak (§11) does not reach them, so phase A leaves them unchanged and phase B converts them with the same live predicate **(author's call)**.

| # | Table | Cap, scope | Enforced at | ms1 rows | Why deferred |
|---|---|---|---|---|---|
| B1 | `profiled_native_registration_history` | 1024 per node, 4096 global; 16 / 64 MiB | trigger 0028:50-58; Rust `closure/history.rs:114-122` | 85 (one per node boot) | grows per restart (1024 restarts per node); needs an age column |
| B2 | `profiled_material_presence_heads` (counter `presence_heads`) | 128 per node, 4096 global | trigger 0029:132-157 | 0 | presence reports are not sent today |
| B3 | `profiled_settlement_evaluations` (`sum(reserved_bytes)`, 192 KiB each) | about 341 global | trigger 0039:76-102; Rust `settlement/owner.rs:270-287` | 0 | human-review settlement path only |
| B4 | `review_owner_budget` (monotonic counter) | about 511 reviews global; 256 per lineage | `reviews.rs:345-354` | 0 | review path only |
| B5 | `execution_receipts` kinds `intelligence_inventory`, `intelligence_failure` | 4096 per node | Rust `intelligence/registry.rs:199-222` | 0 | the current worker does not post this report |
| B6 | `profiled_launch_closures` with `initial_authority_kind='legacy_prepare_v1'` | 256 per node, 4096 global | trigger 0033:124-152 (phase closures return early at :146); Rust `closure/service.rs:226-235` | 0 | legacy launch path |
| B7 | `profiled_execution_receipts` kind `renew` | 4096 per node, 65,536 global, 256 per attempt | Rust `renewal/history.rs:15-17, 217-249` | 0 | legacy renewal path |
| B8 | `delegated_operations` (bytes) | 4 MiB per node, 64 MiB global | Rust `phase_execution/delegated_renewal.rs:206-220`, `delegated_permits/incarnation.rs:139-148` | 0 | delegated renewal not used yet |

### 5.3 Caps that are already bounded by something live (unchanged)

`profiled_launch_artifacts` 128 per attempt (0020:70-71), `profiled_result_reports` 64 per attempt (0024:36), settlement evaluations 16 per attempt (0039:85-87), `agent_graph_operations` 4096 per attempt, `native_registration_receipts` 1024 per (node, boot), result media 8 per report revision, playbook host operations 256 per lineage, source transfers 1024 unreleased (`source_retention.rs:388-395`). Admin-path caps (project bindings, profile catalogs, policies, intelligence operations) grow with operator actions, not with work, and are out of scope.

### 5.4 Order of the stops

For one long-lived node in one project, the stops come in this order: A1 phase-terminal capacity at about 17 attempts, A2 admission artifacts at 33, A3 preparation bytes at about 54. Meanwhile A4 presence operations stop the node after about 47 hours of running, whatever the attempt count. The 0007 acceptance spread its 33 attempts over 12 nodes because the workers were re-enrolled several times, so A2 was reached first.

### 5.5 Delete guards and pins a deletion has to pass

There are about 30 delete triggers that refuse unconditionally (class a). Five are conditional or pin checks (classes b and c). Only one trigger function honours a deletion setting: `observation_retention_delete_guard` (0064:41-54, `aq.row_retention`). The full table is in Appendix A. The main ones:

- `profiled_sealed_row_guard` (0013:124) on `profiled_launches`, `intelligence_decisions`, `intelligence_attempt_selections`, the started, stopped, never-granted and verified-release receipts, result reports and cancellations: "sealed admission record cannot be deleted". The store attests this body in `store/cancellation43.rs`, `store/result38_catalog.*` and `store/settlement39_catalog.json`.
- `admission_inline_immutable` (0020:8) on `admission_artifacts`, `admission_inline_receipts`, `profiled_launch_artifacts`.
- `refuse_profiled_closure_history_change` (0028:45), `refuse_profiled_presence_history_change` (0029:67), `refuse_profiled_v2_history_change` (0030:26), `profiled_phase_execution_guard` (0032:24), `profiled_phase_terminal_capacity_guard` (0033:15), `profiled_source_export_guard` (0022:16), `profiled_hot_guard` (0015:46), `profiled_owner_journal_guard` (0021:22), `settlement_evaluation_guard` (0039:76), `settlement_completion_guard` (0065:33), `result_feedback_immutable` (0044:106), `delegated_permit_guard` (0050:19), `profiled_replacement_release_guard` (0052:15), `profiled_stopped_disposition_guard` (0066:17).
- Pin checks: `settlement_frozen_basis_guard` (0039:165, 192, 203) refuses deleting bindings, Graph operations, source transfers and mail that an accepted settlement used; `source_base_reservation_guard` (0022:99) refuses deleting a base that names an export; `settlement_review_profile_guard` (0039:10).
- `profiled_auto_settlement_owner` (0059:48) returns `NEW` on DELETE. NEW is NULL in a DELETE trigger, so a delete is silently skipped. Phase B fixes this.
- Cascades: `stall_episodes`, `stall_nudges`, `stall_quarantines`, `question_decisions` (and `stall_actions` from episodes) are `ON DELETE CASCADE` to attempts. A cascade silently lifts a quarantine (§8.2).

All FKs are `NOT DEFERRABLE ON DELETE NO ACTION`, except the deferred cycle between `profiled_settlement_evaluations` and `profiled_verified_releases`, so both are deleted in one transaction. The children-first order for one run is in Appendix B.

## 6. When a row is live

### 6.1 A live run

A run (attempt) is **live** while its row in `execution_attempts` holds its task, or its status is not terminal:

```sql
e.holds_task OR COALESCE(h.state->>'status', e.state->>'status', 'unknown')
                NOT IN ('finished','declined','never_granted')
-- e = execution_attempts, h = profiled_authority_hot (LEFT JOIN on attempt_id)
```

This is the predicate DEP6 row retention already uses (`crates/aq-row-retention/src/lib.rs:38-40, 49-52`), so the server has one definition **(author's call)**. A finished run that still holds its task (awaiting settlement) is live. On ms1, 11 of 33 runs are live: 5 granted, 6 finished but still holding.

The SQL function `sealed_attempt_live(attempt_id)` (migration 0103) is that predicate.

### 6.2 A pending row

The poll writes some rows before the attempt exists: selection decisions, admission artifacts, source exports. Others are written by a poll that fails before publication and are never linked. A row that no run references yet is **pending** for 10 minutes after it was written. A pending row counts as live **(author's call)**. Ten minutes is twenty times the 30 s selection TTL (`aq-selection/src/select.rs:118`) and covers any single poll's offer assembly. After that an unlinked row is an orphan: it no longer counts, and phase B deletes it.

### 6.3 Per table

| Table | How a row maps to its run | Live when |
|---|---|---|
| `admission_artifacts` | `profiled_launch_artifacts.artifact_id` → `attempt_id`. A row can be shared: on ms1, 77 rows are linked by more than one run (up to 23). Owner key = project | any linking run is live, or no run links it and `created_at_ms` is within 10 minutes |
| `intelligence_decisions` | `profiled_launches.decision_id` → `attempt_id` (1:1 today) | the linking run is live, or no run links it and `decision.inputs.now_ms` is within 10 minutes |
| `profiled_source_exports` | `profiled_launch_source_exports.operation_id` → `attempt_id`; `source_bases.profiled_export_operation_id` → `attempt_id` | `state = 'pending'`, or any run linked through either table is live |
| Staging bytes of an export | as above | the export is live |
| `admission_material_leases` | `admitted_attempt_id` | the run is live |
| `admission_material_receipts` | `lease_id` → lease | its lease is live |
| `profiled_v2_proposals`, `profiled_v2_accepts`, `profiled_phase_execution_grants`, `profiled_phase_terminal_capacity`, `profiled_launch_registrations` | `attempt_id` | the run is live |
| `profiled_execution_receipts` (legacy) | `(node_id, operation_id)`, no run column | always (unchanged; zero rows on the native path) |
| `profiled_presence_operations` | `attempt_id` for kind `presence`; none for `inventory_v2` | not yet expired (`expires_at_ms` is 29 s after the report), or kind `presence` with a live run |
| `profiled_native_registration_history` (phase B) | `profiled_launch_registrations.registration_id` | a live run's registration references it, or it was written within 24 hours |

For deletion (phase B) a run's records are eligible once the run is not live **and** its task is closed: §8.1.

## 7. Phase A: live-only caps, a rate limit, visible holds (unblocks 0007 phase 1)

Phase A deletes nothing.

### 7.1 Every phase A cap counts only live rows

The cap values stay as they are: the same rows and bytes, per the same scope. Only the counted set changes, from "every row ever" to "live rows" (§6) **(author's call: keep the values; with live counting they become concurrency backstops, far above what a swarm holds at once)**.

One SQL function, `sealed_history_usage(kind, scope, now_ms, exclude)` (migration 0103), computes live usage for every kind. The trigger, the Rust mirror check, the poll gate (§7.3) and the Swarm read (§7.4) all call it, so they cannot drift apart. Its kinds:

| Kind | Scope argument | Replaces |
|---|---|---|
| `admission_artifacts` | project | A2 |
| `intelligence_decisions` | none (nil) | A5 |
| `source_exports` | project | A9 rows and metadata bytes |
| `staging_exports` | none | the export part of A10 |
| `native_preparation` | node | A3 bytes and A6 rows |
| `v2_proposals` | node | A7 |
| `launch_registrations` | node | A8 |
| `phase_terminal` | node | A1 |
| `presence_operations` | node | A4 live rows |
| `presence_rate` | node | A4 reports in the last hour (§7.2) |
| `offers` | node | the offer rate per node and per server (§7.2) |
| `project_offers` | project | the offer rate per project (§7.2) |

Each returns `(total_rows, total_bytes, scope_rows, scope_bytes)`. "Now" is always passed in, never read from the database clock, so it is in the server clock that the injected test clocks drive: triggers pass the incoming row's own timestamp (`NEW.created_at_ms`, `NEW.recorded_at_ms`, the decision's `inputs.now_ms`, the offer's source observation `observed_at_ms`), and Rust passes `now(state)`.

How each enforcement site changes:

| Cap | Change |
|---|---|
| A1 | Migration 0104 replaces `profiled_phase_terminal_capacity_guard`: the count and byte sums come from `sealed_history_usage('phase_terminal', NEW.node_id, 0)`. The DELETE and UPDATE refusal stays. |
| A2 | Migration 0104 replaces `admission_artifact_insert_guard`: the two `count(*)`/`sum` queries become `sealed_history_usage('admission_artifacts', NEW.owner_key::uuid, NEW.created_at_ms)`. Every other check in the body is kept byte for byte. |
| A3, A6 | `preparation::retained_quota_locked` (`preparation.rs:480-491`) returns `aq_history_caps::preparation_usage(..)`, which reads `sealed_history_usage('native_preparation', node, 0)`. The tuple shape stays the same, so the four call sites are unchanged. |
| A4 | Migration 0104 replaces `profiled_presence_operation_guard`: the cap condition uses `sealed_history_usage('presence_operations', NEW.node_id, NEW.recorded_at_ms)`, adds the hour limit (§7.2), and keeps updating the storage counters as statistics. |
| A5 | `resolver.rs:340-351` reads `aq_history_caps::usage(.., Kind::IntelligenceDecisions, nil, decision.inputs.now_ms)`. |
| A7 | `accepted_preparation.rs:65-72` reads `aq_history_caps::usage(.., Kind::V2Proposals, node, 0)`. |
| A8 | Migration 0104 replaces `profiled_launch_registration_guard` (count from `sealed_history_usage('launch_registrations', NEW.node_id, 0)`); `closure/history.rs:151-156` uses the same function through the crate. |
| A9 | Migration 0104 replaces `profiled_source_export_guard`: the row and metadata sums count live exports, excluding `NEW.operation_id` as before. `source_exports.rs:340-355` uses the crate. |
| A10 | The staging sum (`sealed_staging_live_bytes()`), used at `source_exports.rs:331`, `source_retention.rs:397` and `source_retention.rs:915`, counts `charged_bytes` of live exports only; the transfer and source-base terms are unchanged **(author's call: aq-git owns the bundle bytes, and source GC with `keep_per_task` 2 and `max_age` 30 days bounds them)**. |

Rust mirrors must keep working at older state formats: the frozen-profile test fixtures (for example `with_state32_store`) issue grants before the database reaches 0103. The new crate `aq-history-caps` therefore checks `to_regprocedure('sealed_history_usage(text,uuid,bigint,uuid)')` and falls back to the old all-rows query when the function is absent. The old SQL strings move from the aq-server lib into the crate with that fallback, which also pays for the lines the gate adds.

### 7.2 The rate limit

A lifetime bound is replaced by a bound on the rate. Once phase B deletes records, storage is then bounded by "rate × (retention + time until the task closes)". The unit is the **offered attempt**, because every capped table in §5.1 except A4 grows per offered attempt, and an attempt is what the owner sees **(author's call)**.

| Limit | Value | Reasoning |
|---|---|---|
| Offers per node per rolling hour | **30** | A slot runs one agent at a time. The fastest window measured on ms1 was 6 offers in 20 minutes on one node (node f85cc3e1, 16:30–16:50 on 2026-11-08), during an acceptance script built from short tasks and handoffs: 18 an hour. 30 leaves headroom over that and is one offer every 2 minutes, sustained. A loop where offers fail immediately (one per poll) is held within the hour. Lineage budgets still bound attempts per task. |
| Offers per project per rolling hour | **240** | Eight slots, each at its node limit. |
| Offers per server per rolling hour | **960** | Four projects at their limit. |
| Inventory reports (A4) per node per rolling hour | **720** | One every 5 s. Measured: one every 41 s typically, one every 11 s at the peak (333 an hour). |
| Inventory reports per server per rolling hour | **11,520** | Sixteen nodes at their limit. |

Derived row rates at the node limit, from the measured rates per attempt: 444 admission artifacts, 95 decisions, 30 leases, 30 registrations and 27 exports an hour per node. On disk, ms1's sealed tables take 9.8 MB for 33 attempts, about 300 KB per attempt including indexes. A sustained server-wide runaway at 960 offers an hour would therefore add about 290 MB an hour; the hold stops it after the first hour.

The limits are constants in the SQL function `sealed_history_limits()` (migration 0103), not settings **(author's call: the owner asked only for the retention period in Settings; a rate that needs raising is a migration with a reason)**.

Where the rate is enforced:

- **The poll gate** (§7.3) refuses before any history row is written.
- **A database fence** (migration 0105): a `BEFORE INSERT` trigger on `profiled_launches`, `profiled_launch_offer_rate`, counts offers in the hour before the new offer's source observation, per node, per project and in total, under an advisory lock, and raises `sealed history offer rate`. Only a race between two polls that both passed the gate can reach it, and the next poll's gate then holds visibly.
- The inventory-report rate is enforced in `profiled_presence_operation_guard` (A4).

Orphan rows (§6.2) are also rate-bounded: pending rows count toward their table's live cap, so at most a cap's worth of orphans can be written per 10 minutes.

### 7.3 The poll gate

A new crate, `crates/aq-history-caps`, owns the gate. `offers()` in `profiled_execution/producer/workflow.rs` calls it after the existing `offer_hold` and the `busy` check, and before any selection, export or artifact write:

1. **Node and server holds** (`aq_history_caps::node_holds(conn, node, now_ms, staging)`): the offer rate for the node and the server; live usage plus headroom against each node-scoped and global cap (A1, A2 global, A3, A5, A6, A7, A8, A9 global, A10); inventory reports for the node. Any hold → the poll returns its page with no offers (200), logs `"{code} {subject}: {used} of {limit}; no new offers"` at info, and writes nothing.
2. **Project holds** (`aq_history_caps::project_holds(conn, project, now_ms)`): inside the fair-share loop, a project with a hold (its offer rate, A2, A9) is skipped like a project with no ready task, so other projects still get offers.

Headroom is the most a single offer can add: 64 admission artifacts and 4 MiB, 2 decisions and 16 MiB, 1 export and 4 MiB, 1 lease and 256 KiB of preparation bytes, 1 registration, 1 phase reservation and 1 MiB, and `max_bundle_bytes` of staging **(author's call)**. With headroom, a refusal happens at the gate and not halfway through a poll.

### 7.4 Visible holds

`GET /api/v1/swarm` computes the same holds: `aq_history_caps::swarm_holds(conn, node_ids, now_ms, staging)`, which returns node holds for each listed node and project and server holds for every enabled project. The holds go into `SwarmInput.history`. Then:

- a node hold is appended to that node's `holding`, so its machine shows `holding` (the existing rule);
- a project or server hold is appended to `waiting.reasons`.

Two new `HoldCode` values (contract `dashboard-read-1`):

| Code | `message` | `remedy` |
|---|---|---|
| `history_rate_limited` | `Offer rate limit reached: {used} offers in the last hour (limit {limit})`, or `Inventory report rate limit reached: {used} reports in the last hour (limit {limit})` | `Wait for the hour to pass; if this repeats, look for a task that fails in a loop` |
| `history_cap_reached` | `Live {subject} cap reached: {used} of {limit}` (with ` bytes` appended for a byte limit). The subject is one of `admission artifacts`, `selection decisions`, `source exports`, `staging bytes`, `preparation bytes`, `material leases`, `preparation proposals`, `launch registrations`, `phase terminal reservations` or `inventory reports` | `Wait for running agents to settle` |

A project hold's message starts with `Project {project_id}: ` and a server-wide hold's with `Server: `, followed by the text above with a lower-case first letter, for example `Project 0000…0001: live admission artifacts cap reached: 449 of 512`.

The dashboard adapter already tolerates unknown codes, so an older dashboard shows the message text. The re-pin still merges together with the server change (owner rule, retrospective W76).

### 7.5 Migrations

The current maximum on main is 0102. Phase A adds three, contiguous:

| Migration | Contents |
|---|---|
| `0103_sealed_history_live.sql` | `sealed_attempt_live(uuid)`, `sealed_history_limits()`, `sealed_history_usage(text,uuid,bigint,uuid)`, `sealed_staging_live_bytes()`, `sealed_history_holds(uuid,uuid,bigint)`; indexes `profiled_launch_artifacts(artifact_id)`, `profiled_launches(decision_id)`, `profiled_launches(source_observation_id)`, `admission_source_observations(observed_at_ms)`, `admission_material_leases(admitted_attempt_id)`, `profiled_presence_operations(node_id, recorded_at_ms)`, `source_bases(profiled_export_operation_id)`. No table changes. |
| `0104_sealed_history_live_caps.sql` | `CREATE OR REPLACE` of `admission_artifact_insert_guard`, `profiled_source_export_guard`, `profiled_launch_registration_guard`, `profiled_phase_terminal_capacity_guard` and `profiled_presence_operation_guard`. Names, triggers and every non-quota check are unchanged (the store checks the phase-terminal function by name, `store.rs:314-345`). |
| `0105_sealed_offer_rate.sql` | `profiled_launch_offer_rate()` and its `BEFORE INSERT` trigger on `profiled_launches`. |

The exact bodies are in the phase A plan, `docs/superpowers/plans/2026-11-08-sealed-history-p-a-live-caps.md`.

## 8. Phase B: deletion after the task closes, and the summary

### 8.1 Eligibility

A run's full records are eligible for deletion when all of the following hold, checked in the deleting transaction **(author's call where not (owner))**:

1. the run is not live (§6.1);
2. its task is closed, and has been closed, with every attempt of the task not live, for longer than the retention period **(owner)**. The clock is DEP6's `retention_tasks.settled_since_ms`: the first time the server saw a fresh Graph read of the task as completed, with all its attempts released and terminal. It was re-checked in this pass (`aq-row-retention/src/lib.rs:19-56`). A reopen resets it. The period is `retention_days`, default 7 **(owner)**;
3. source GC has finished with the run: no `source_transfers` row of the run has unreleased staging, and the run's task is past `source_retention/gc.rs`'s settled sweep. Source GC reads completions, auto-settlements and transfers, so it must run first;
4. no open review decision references the run's results;
5. no active stall quarantine is held only by this run's rows (the quarantine is copied to the summary first, §8.2).

Shared rows are deleted separately, when nothing references them and they are not live: admission artifacts linked by no run, decisions, exports, registration history, and source observations older than the retention cutoff.

### 8.2 The summary row (~1 KB, kept forever)

The table is `attempt_summaries`, with one row per deleted run. Its columns: `attempt_id` (PK), `task_id`, `workspace_id`, `project_id`, `node_id`, `session_id`, `generation`, `deleted_at_ms`, `retention_days`, and `summary JSONB` with `CHECK(octet_length(summary::text) <= 2048)`. The sweeper writes it in the same transaction as the delete, so there is never a moment with neither **(author's call)**. Fields in `summary` (version 1), chosen from the readers below:

- **Times:** `offered_at_ms`, `granted_at_ms`, `started_at_ms`, `stopped_at_ms`, `settled_at_ms`.
- **State:** `status` (finished, declined, never_granted), `stop_kind`, `cancelled`.
- **Who ran it:** `provider`, `model`, `harness`, `intelligence_profile`, `task_revision`, `machine_key`.
- **Outcome:** `report_id`, `outcome_state`, `conclusion` (first 200 characters of the worker-reported conclusion), `result_digest`, `result_commit`, `files_changed`.
- **Settlement:** `disposition`, `settled_by` (auto or human), `review_decision`.
- **Usage:** `input_tokens`, `output_tokens`, `cached_tokens`, `priced_microunits`, `currency`, `usage_complete`, `sandbox_ms`, `vm_microunits`, `vm_started_at_ms`, `vm_stopped_at_ms`.
- **Lineage (the replacement `Prior` fields):** `predecessor_attempt_id`, `handoff_reason`, `exempt`, `lost_charged`, `released`, `rework`, `unpublished`, `completion_state`, `never_granted`, `quarantined`.
- **Evidence digests:** `manifest_digest`, `execution_spec_digest`, `selection_digest`, `decision_payload_digest`. These prove which sealed record existed, without its bytes.

What each reader reads after deletion. The readers come from the read inventory of 2026-11-08; file:line refers to main at 0102.

| Reader | Today | After deletion |
|---|---|---|
| Overview attempt counts (`dashboard_api/aggregation.rs:463-519`) | running and recovering runs only | unchanged; deleting settled rows also fixes its 101-row window |
| Overview settlements (`aggregation.rs:705`) | newest 26 evaluations per actor | deleted runs drop off; they are more than 7 days past closing **(author's call)** |
| `/api/v1/sessions` (`dashboard_api/service.rs:593`) | settled sessions listed | the union of live attempts and `attempt_summaries` |
| `/api/v1/sessions/{id}` (`service.rs:707-829`, `lifecycle.rs:8`) | full detail | an `archived` session built from the summary: identity, profile, times, status. No terminal, no operations (404 for those) |
| Session usage (`observations/inspection.rs:28-204`) | projections and priced facts | the summary's usage totals with `usage_complete` |
| Task history (`agent_access/human/work.rs:178-206`) | attempt_id, session_id, holds_task, status (max 16) | the union with summaries (holds_task false) |
| Results feed and detail (`results_feedback/store.rs:830`, `profiled_execution/results.rs:311-447`) | re-verifies the full receipt chain per item; one missing item fails the page | archived items are rendered from the summary's outcome fields without re-verification; the detail page shows the archived card |
| Reviews (`reviews.rs:158-747`) | while a decision is open | not affected: a run with an open decision is not eligible |
| Notifications `reconcile_tasks` (`notifications/sources.rs:229`) | latest result per task | it must treat a run that moves to summaries as unchanged; phase B tests this |
| Replacement history (`replacement.rs:57-175`, `aq-profiled-state/src/replacement_sql.rs:6-25`) | full `Prior` for every attempt of the task; a count mismatch blocks the task (`replacement_lineage_history_unknown`) | the union of live rows and summary `Prior` fields, so a reopened task keeps its budgets |
| Never-granted cap and generations (`no_grant.rs:273-322`, `resolver.rs:59`) | count and max(generation) of the task's attempts | the union with summaries (`generation`, `never_granted`), so generations are never reused |
| Node spend ceilings (`placement.rs:35-56`, `observations/budget.rs:74`, `vm_time.rs:102-110`) | priced facts and `accounting_vm_time` in the month window | the union with the summary's `priced_microunits` and `vm_*`, so current-month spend is not under-counted |
| Stall quarantine (`replacement.rs:111`) | `stall_quarantines` (cascades on attempt delete) | the summary's `quarantined` flag |
| Playbook host (`playbooks/host_adapters.rs:947-1110`) | project_id, selection_digest of past attempts | the summary |
| `/api/v1/swarm`, capacity | live only | unchanged |

### 8.3 Relaxing the guards safely

- **A ticket.** In the deleting transaction, the sweeper first inserts `sealed_history_purges(attempt_id, cutoff_ms, txid)` with `txid = txid_current()`. A `BEFORE INSERT` guard on that table re-checks §8.1 items 1 and 2 against the cutoff and refuses otherwise.
- **A setting.** The sweeper sets `SET LOCAL aq.sealed_retention = 'on'`, following the `aq.row_retention` pattern of 0064.
- **Each class-a delete guard** on a per-run table changes from "refuse" to "refuse unless `sealed_purge_allowed(OLD.attempt_id)`". That function is true only when the setting is on and a ticket for that attempt exists in the current transaction. UPDATE refusals and TRUNCATE guards do not change.
- **Shared tables** (`admission_artifacts`, `admission_inline_receipts`, `intelligence_decisions`, `profiled_source_exports`, `profiled_native_registration_history`, `admission_source_observations`) allow a delete only when the setting is on, no reference remains, and the row is older than the cutoff.
- **Pin checks** (class c) stay. The order deletes the referencing rows first, so they pass.
- **The `profiled_auto_settlement_owner` DELETE bug** (§5.5) is fixed to return `OLD` under the ticket.
- **Store attestations.** The store checks some guard bodies by format: `profiled_sealed_row_guard` in `store/cancellation43.rs`, `result38_catalog`, `settlement39_catalog`. Phase B adds format-gated overrides, as 0102 did for `native_incarnation_guard` (`store/compute41.rs`).
- **AQ-08.** `docs/implementation/AQ-08-authority-bridge.md:195-197` is amended to name this policy as the qualified maintenance policy.

### 8.4 The sweeper

- **Where it runs.** A new module in `crates/aq-row-retention` (`sealed.rs`), run by `row_retention::run` on its existing 10-minute tick, after DEP6's `collect`, so the `retention_tasks` clocks are fresh **(author's call)**.
- **Batches.** One transaction per run, under `statement_timeout = 10s`. At most 8 runs per pass, and shared-row cleanup of at most 512 rows per table per pass **(author's call)**. A backlog drains over later ticks. An error rolls back that run and logs it, and the run is retried on a later tick.
- **One run's transaction:**
  1. lock the run's attempt row;
  2. re-check §8.1;
  3. compute and insert the summary;
  4. insert the ticket;
  5. delete in the order of Appendix B, including the run's observation rows after folding their totals into the summary.

  Step 5's observation rows are deleted at the retention period, not DEP6's 14 days. They are part of the run's full record, and the FK to `execution_attempts` forces it **(author's call)**.
- **The setting.** It lives in a singleton table, `sealed_history_settings(retention_days INT NOT NULL DEFAULT 7 CHECK (retention_days BETWEEN 1 AND 365), generation, updated_by, updated_at)`. The dashboard action `history.retention.set` changes it with a compare-and-swap on `generation`, the pattern of `swarm.availability.set` (`intelligence/availability_action.rs`). Settings shows "Keep full run records for N days after the task closes". The dashboard-read contract adds `history.retention_days`.
- **Logs.** `table` and `rows_deleted` for every table, and `runs_summarised`.
- **Phase B also converts the caps in §5.2** to the live predicate.

## 9. Compatibility impact

- **Worker wire contracts** (aq-admission pinned baselines `control.compatibility-sha256.json`, `granted-closure.compatibility-sha256.json`, `preparation-v2.compatibility-sha256.json`; aq-session-protocol 0.1.0; aq-protocol 0.1.2): no byte changes in either phase. Phase A answers a held poll with an empty page, a shape the worker already handles. Phase B changes one meaning: an exact replay of an operation of a deleted run is refused instead of returning the original receipt. Nodes replay only within operation lifetimes, which are well under the retention period. The aq-admission README records that replay proof is retained for the retention period after the task closes.
- **`dashboard-read-1`** (`protocol/dashboard-read/v1/schema.json`, generated by `cargo run -p aq-server --example dashboard_contract`):
  - phase A adds two `HoldCode` values;
  - phase B adds `archived` session and result shapes and `history.retention_days`.

  Each is additive. Each merges together with the aq-dashboard re-pin (`scripts/pin-contract.py`, `src/api/wire/swarm.ts`), per the owner's rule.
- **`dashboard-actions`**: phase B adds `history.retention.set`.
- **Contract pins in the dashboard bundle** (`bundle-requirements.json`): neither phase adds a requirement. An older server simply never sends the new codes.
- **Store format:**
  - phase A raises the state format to 105. Readable formats still include all older ones, so a rollback of the binary before 0103 runs is safe. After 0103, the new functions are additive, and an older binary ignores them;
  - phase B's deletions cannot be undone, by design. A rollback after phase B leaves an older binary looking at missing runs. The phase B plan states the supported rollback window.
- **Build budgets:**
  - phase A's new code goes into `aq-history-caps`, and the moved mirror SQL pays for the gate. The aq-server lib ratchet (109,185) only goes down;
  - no test binary is added.

## 10. Constraints

- The AGENTS.md build rules: `timeout` on every cargo test, tests in the eight existing binaries, nextest groups for selection fixtures, and the ratchet only going down.
- No time estimates in documents, code comments or PR text.
- Migrations stay contiguous from 0103.
- Nothing here reaches an agent sandbox.

## 11. Acceptance

### Phase A

1. All workspace tests pass, including the new ones the plan names.
2. Clippy, fmt, `scripts/build-budget.py` and the dashboard-read `--check` pass.
3. **Live soak on aq-ms1:**
   - the stack runs origin/main with phase A merged, and its database is the one that stopped on 2026-11-07 (512 admission artifacts). The first poll after the upgrade gets an offer;
   - a soak project of trivial tasks ("write a file and finish") is driven through the workers until **at least 650 offered attempts** are recorded in `profiled_launches` since the upgrade, with at least one node taking more than 300 of them;
   - **pass condition:** no poll fails with a cap or quota error. Check this two ways: no server log line "native control request refused" whose reason names a quota or capacity, and no attempt stuck between offer and grant;
   - every rate-limit hold during the soak appears as `history_rate_limited` on that node in `GET /api/v1/swarm` and on the Swarm page, and clears when the hour's count drops;
   - **deliberate hold:** with the soak stopped, set the node's slot count so that one machine offers above 30 in an hour (or seed rows as the plan's test does on a copy of the database). The Swarm page then shows the node `holding` with "Offer rate limit reached", and the worker log shows empty polls, not 409s;
   - the evidence (counts per table before and after, the swarm read JSON with the hold, the log excerpts) is written to `docs/implementation/0008-PA-acceptance.md`.

### Phase B

1. On a copy of the soak database with the retention period set to 1 day:
   - a sweep deletes every run whose task has been closed for more than 1 day;
   - after the sweep, each such run has exactly one `attempt_summaries` row of at most 2048 bytes (average about 1 KB);
   - no live run and no open task loses a row.
2. After the sweep, these all return 200 and show the deleted runs from their summaries: task history, the sessions list, an archived session, the results feed, an archived result and session usage.
3. Reopening a closed task whose runs were deleted gives it the same replacement budget it had before deletion.
4. Every guard refuses a delete outside the sweeper's ticket.

## 12. Delivery phases

| Phase | Work | Depends on |
|---|---|---|
| A. Live caps (plan `docs/superpowers/plans/2026-11-08-sealed-history-p-a-live-caps.md`) | §7: migrations 0103–0105, `aq-history-caps`, the poll gate, swarm holds, contract re-pin | — |
| B. Deletion and summaries | §8: summaries, tickets, guard relaxation, sweeper, Settings, readers on summaries, §5.2 caps | A |

Phase A is what 0007 phase 1 needs. Phase B can follow when the owner has answered §13.

## 13. Questions for the owner

1. **Result reports and review comments.** A run's result report (its body, evidence and media) and the human review threads on it are linked to the run's sealed records by FKs, so phase B as written deletes them with the run and keeps only the summary's outcome card (§8.2). Is that what you want? The alternative is to keep result reports and their review threads for longer, or forever. That needs the result tables cut loose from the run's records.

## Appendix A. Delete-firing triggers (live catalog at format 102)

| Table | Trigger → function | Class | Defined at | Condition |
|---|---|---|---|---|
| intelligence_decisions, intelligence_attempt_selections, profiled_launches, profiled_verified_releases, profiled_never_granted_closures, profiled_started_receipts, profiled_stopped_receipts, profiled_result_reports, profiled_cancellations, profiled_cancel_operations, profiled_cancelled_settlements | → `profiled_sealed_row_guard` | a | 0013:124 | — |
| admission_artifacts, admission_inline_receipts, profiled_launch_artifacts | → `admission_inline_immutable` | a | 0020:8 | — |
| profiled_authority_hot | `profiled_hot_guard` | a | 0015:46 | — |
| profiled_owner_journals | `profiled_owner_journal_guard` | a | 0021:22 | — |
| native_registration_receipts | `native_registration_receipt_immutable` | a | 0019:10 | — |
| profiled_source_exports | `profiled_source_export_guard` | a | 0022:16 | — |
| profiled_launch_source_exports | `profiled_source_export_link_guard` | a | 0022:74 | — |
| source_bases | `source_base_reservation_guard` | b | 0022:99 | refuses when `profiled_export_operation_id` is set |
| profiled_result_heads | `profiled_result_head_guard` | a | 0024:62 | — |
| profiled_native_registration_history, profiled_launch_registrations, profiled_launch_closures | → `refuse_profiled_closure_history_change` | a | 0028:45 | — |
| profiled_presence_operations | → `refuse_profiled_presence_history_change` | a | 0029:67 | — |
| profiled_material_presence_heads | `profiled_material_presence_head_guard` | a | 0029:132 | — |
| profiled_inventory_semantic_heads | `profiled_inventory_semantic_guard` | a | 0031:20 | — |
| profiled_v2_proposals, profiled_v2_accepts | → `refuse_profiled_v2_history_change` | a | 0030:26 | — |
| profiled_phase_execution_grants | → `profiled_phase_execution_guard` | a | 0032:24 | — |
| profiled_phase_terminal_capacity | `profiled_phase_terminal_capacity_guard` | a | 0033:15 | — |
| profiled_settlement_evaluations | `settlement_evaluation_guard` | a | 0039:76 | — |
| profiled_task_completions | `settlement_completion_guard` | a | 0065:33 | — |
| agent_launch_bindings, agent_graph_operations | → `settlement_frozen_basis_guard` | c | 0039:165 | refuses if an accepted evaluation exists for the attempt |
| source_transfers | `settlement_source_guard` | c | 0039:203 | refuses if an accepted evaluation used the import |
| mailbox_operations | `settlement_mail_source_guard` | c | 0039:192 | refuses if an accepted evaluation's sender matches |
| review_subjects | `settlement_review_profile_guard` | b | 0039:10 | refuses for profile `source_and_effects_review_v1` |
| human_decision_finality | → `review_immutable_record` | a | 0026:63 | — |
| result_revision_pins, result_feedback_intents, result_feedback_links, result_feedback_effects, result_evidence_waivers, result_comment_revisions | → `result_feedback_immutable` | a | 0044:106 | — |
| result_media_intents | → `guard_result_media_intent` | a | 0075:16 | — |
| delivery_intents, delivery_receipts, delivery_publish_dispatches | → `delivery_immutable` | a | 0045:31 | — |
| delegated_permits | `delegated_permit_guard` | a | 0050:19 | — |
| profiled_replacement_releases | `profiled_replacement_release_guard` | a | 0052:15 | — |
| profiled_auto_settlements | `profiled_auto_settlement_owner` | b | 0059:48 | needs `aq.settlement_owner`; its DELETE path returns NULL (silently skipped) |
| profiled_stopped_dispositions | `profiled_stopped_disposition_guard` | a | 0066:17 | — |
| observation_sources, observation_source_revocations | → `refuse_observation_history_mutation` | a | 0014:70 | — |
| observation_events, observation_facts, observation_packets | → `observation_retention_delete_guard` | b | 0064:41 | allowed when `aq.row_retention = 'on'` |
| hibernation_receipts | → `hibernation_immutable_receipt` | a | 0087:41 | — |

These tables have no delete trigger: execution_attempts, profiled_launch_materials, admission_material_leases, admission_material_receipts, admission_source_observations, result_aggregates, result_assessments, agent_launch_grants, agent_created_tasks, execution_receipts, profiled_execution_receipts, attempt_handoffs, lost_attempt_releases, retention_closed_attempts, stall_*, profiled_delivery_evidence, profiled_check_evidence, terminal_sessions and children, credential_material_revisions, observation_call_heads, observation_attempt_projections.

## Appendix B. Delete order for one run (children first)

1. **Result feedback** for the run's reports R:
   - result_comment_revisions → result_comments → result_thread_follows, result_threads;
   - result_assessments, result_feedback_intents, result_feedback_links, result_feedback_effects, result_evidence_waivers;
   - result_follows → result_revision_pins → result_aggregates;
   - result_media_intents.
2. **Settlement:** profiled_task_completions; then profiled_settlement_evaluations with profiled_verified_releases in one transaction (deferred cycle).
3. **Reviews:** review_owner_outbox → human_decision_finality, submissions, deferrals, evidence views → human_decision_requests → review_subjects.
4. **Settlement leaves:** profiled_auto_settlements, profiled_stopped_dispositions, profiled_replacement_releases.
5. **Results:** profiled_result_heads → profiled_result_reports, deleted in one statement so the `supersedes` chain resolves.
6. **Cancellation and lifecycle:**
   - profiled_cancelled_settlements → profiled_cancellations;
   - profiled_cancel_operations;
   - profiled_stopped_receipts → profiled_started_receipts → profiled_owner_journals;
   - profiled_never_granted_closures.
7. **Phase and registration:**
   - profiled_launch_closures;
   - profiled_phase_terminal_capacity → profiled_phase_execution_grants → profiled_v2_accepts → profiled_v2_proposals;
   - profiled_launch_registrations;
   - the node-operation receipts of these rows: native_registration_receipts and profiled_execution_receipts by `(node_id, operation_id)`.
8. **profiled_authority_hot.**
9. **Artifacts:** profiled_launch_artifacts. Shared artifacts that nothing references afterwards go in the shared-row pass.
10. **Materials:**
    - profiled_inventory_semantic_heads, profiled_material_presence_heads → profiled_presence_operations of the run;
    - credential_material_revisions, admission_material_receipts, profiled_launch_materials;
    - admission_material_leases → delegated_permit_current → delegated_permits.
11. **Source links:** profiled_launch_source_exports, source_bases. The exports themselves go in the shared-row pass.
12. **Launch:** profiled_launches, intelligence_attempt_selections. The decision and the source observation go in the shared-row pass.
13. **Agent binding:** agent_graph_outbox → agent_graph_operations, agent_launch_grants, agent_created_tasks, agent_created_records, agent_read_cursors, agent_operation_cursors → agent_launch_bindings.
14. **Transfers and terminals:** source_retention_releases → source_transfers; terminal children → terminal_sessions.
15. **Observation:**
    - observation_facts → observation_events, observation_call_heads, observation_attempt_projections;
    - observation_source_revocations → observation_sources;
    - remove the attempt from `observation_packet_retention.attempt_ids`.
16. **Remaining children:** execution_reports, execution_directives, execution_permits, mailbox_receipts, mailbox_deliveries, profiled_check_evidence, profiled_delivery_evidence, retained_artifacts, accounting_vm_time, hibernation_receipts → hibernation_suspensions, attempt_handoffs, lost_attempt_releases, retention_closed_attempts.
17. **execution_attempts.** Stall rows and question_decisions cascade; the quarantine flag is already in the summary.
