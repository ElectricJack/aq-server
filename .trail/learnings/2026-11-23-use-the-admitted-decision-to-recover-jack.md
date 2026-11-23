---
title: Use the admitted decision to recover a stalled delivery run
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
cost: { minutes: 2670, tokens: 10412224 }
id: l-7b4ffbc0
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/delivery_decisions.rs
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-server/src/playbooks/host_adapters/delivery_decisions.rs
  - crates/aq-server/src/profiled_execution/reconciliation/mod.rs
  - crates/aq-server/src/playbooks/host_adapters.rs
  - crates/aq-server/src/profiled_execution/producer/workflow.rs
---

## Decided
Resume a stuck work.return or work.complete through the original playbook intent and the bridge's admitted row, even after the engine's first dispatch deadline. The deadline still gates new admissions. Retry the same result and verify the original grant, current run and settlement fences. Feed the outcome back to the engine so it can take the next step.
A phase terminal reservation protects remaining terminal writes for an issued grant. Release that reservation on the retained phase stop or no-start closure receipt. Keep the task claim and all result and source evidence until their separate delivery and replacement owners finish.
A namespace owner can approve one already-admitted delivery decision in the same transaction that retains a fresh grant with the original scope. When the ordinary recovery pass sees it, the bridge may settle that original intent after rechecking the new grant, current owner, engine run and native result.
Charge staged offer artifacts through open assembly pins and charge published artifacts through live attempt links. Close the stage after all publication retries. Retain the artifact and its receipt. Expiry releases the stage charge after a crash, and a later assembly must book a new charge before it can use the same bytes.

## Tried and ruled out
A bounded retry in the request handled a brief lock timeout and could not recover a pending decision after restart. Selecting only pending bridge rows would also miss a final receipt whose reply was lost before the engine recorded it. Take both from unfinished runs. In the PostgreSQL engine, read exact candidate records through the bounded transition reader so a busy namespace does not need a full snapshot on each recovery pass.
MS1 reported zero running agents while both online nodes hit 16 of 16 terminal reservations. The 32 charged rows had 25 stop receipts and seven launch closure receipts. The general live history set included stopped attempts that still held a ticket and closed grants whose hot status still said cancelling. Counting by that set alone blocked the very poll needed to admit their replacements. Increasing the cap would postpone the same stall.
Replacing the old grant identity on the effect would break the engine's retained intent fingerprint and lose provenance. Accepting any newly created grant with the same scope would silently reactivate other decisions. The new authorization names the exact pending operation and leaves its original authority and payload intact.
Live MS1 accumulated 171 unlinked records during a burst of offers even with bounded publication retries. The ten-minute legacy window charged those records alongside live attempts. One failed assembly cannot abandon a shared artifact, because another assembly can pin the same descriptor and a published attempt can already link it. The first SQL draft still matched staged records through the legacy arm, even after both pins expired. Keep the legacy arm for unscoped rows and the stage arm for the current pins.

## Watch out
In the code: playbook_effects records operation IDs, grant revisions and the command input digest. Reconciliation must find the exact existing row with the exact engine intent. A final receipt can be read back. A pending effect needs current write authority and goes through the same native result owner. A stopped predecessor may have been superseded while its playbook remains open, so do not equate pending decision count with active held tickets.
In the code: compare sealed_history_usage for phase_terminal with profiled_phase_terminal_capacity and the qualified profiled_stopped_receipts or profiled_launch_closures. Keep unresolved reservations and other live history kinds charged. A never-granted Prepare has a different proof from the no-start closure of an issued grant. The worker must look up its exact original authority before releasing a slot.
Revoking the most recent approval's grant makes the operation wait again. The engine still fences a canceled run and disabled artifact. A committed response remains readable, and only pending work.complete or work.return uses the new grant to make a write.

In the code: the grant request's resume_delivery_operation_id is included in the grant request digest. The migration records the approval separately and looks up only the latest approval for the operation.
Quota transitions for reused artifacts take the same advisory lock as new artifact inserts. Keep published live links in the usage query and require an exact stage pin on a staged artifact before publication links it. Publication and its retries finish before stage close. A lost database reply still leaves an actual committed live link charged.

In the code: 0167_release_offer_stage_capacity.sql owns the usage definition and the pin/link constraints. offer_stage.rs cleans up pin rows while leaving the underlying retained objects intact.
