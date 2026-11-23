---
title: Use the admitted decision to recover a stalled delivery run
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
cost: { minutes: 1763, tokens: 6933455 }
id: l-7b4ffbc0
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/delivery_decisions.rs
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-server/src/playbooks/host_adapters/delivery_decisions.rs
  - crates/aq-server/src/profiled_execution/reconciliation/mod.rs
---

## Decided
Resume a stuck work.return or work.complete through the original playbook intent and the bridge's admitted row, even after the engine's first dispatch deadline. The deadline still gates new admissions. Retry the same result and verify the original grant, current run and settlement fences. Feed the outcome back to the engine so it can take the next step.
A phase terminal reservation protects remaining terminal writes for an issued grant. Release that reservation on the retained phase stop or no-start closure receipt. Keep the task claim and all result and source evidence until their separate delivery and replacement owners finish.

## Tried and ruled out
A bounded retry in the request handled a brief lock timeout and could not recover a pending decision after restart. Selecting only pending bridge rows would also miss a final receipt whose reply was lost before the engine recorded it. Take both from unfinished runs. In the PostgreSQL engine, read exact candidate records through the bounded transition reader so a busy namespace does not need a full snapshot on each recovery pass.
MS1 reported zero running agents while both online nodes hit 16 of 16 terminal reservations. The 32 charged rows had 25 stop receipts and seven launch closure receipts. The general live history set included stopped attempts that still held a ticket and closed grants whose hot status still said cancelling. Counting by that set alone blocked the very poll needed to admit their replacements. Increasing the cap would postpone the same stall.

## Watch out
In the code: playbook_effects records operation IDs, grant revisions and the command input digest. Reconciliation must find the exact existing row with the exact engine intent. A final receipt can be read back. A pending effect needs current write authority and goes through the same native result owner. A stopped predecessor may have been superseded while its playbook remains open, so do not equate pending decision count with active held tickets.
In the code: compare sealed_history_usage for phase_terminal with profiled_phase_terminal_capacity and the qualified profiled_stopped_receipts or profiled_launch_closures. Keep unresolved reservations and other live history kinds charged. A never-granted Prepare has a different proof from the no-start closure of an issued grant. The worker must look up its exact original authority before releasing a slot.
