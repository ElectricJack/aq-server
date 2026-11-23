---
title: Use the admitted decision to recover a stalled delivery run
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
cost: { minutes: 885, tokens: 3471951 }
id: l-7b4ffbc0
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/delivery_decisions.rs
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-server/src/playbooks/host_adapters/delivery_decisions.rs
---

## Decided
Resume a stuck work.return or work.complete through the original playbook intent and the bridge's admitted row, even after the engine's first dispatch deadline. The deadline still gates new admissions. Retry the same result and verify the original grant, current run and settlement fences. Feed the outcome back to the engine so it can take the next step.

## Tried and ruled out
A bounded retry in the request handled a brief lock timeout and could not recover a pending decision after restart. Selecting only pending bridge rows would also miss a final receipt whose reply was lost before the engine recorded it. Take both from unfinished runs. In the PostgreSQL engine, read exact candidate records through the bounded transition reader so a busy namespace does not need a full snapshot on each recovery pass.

## Watch out
In the code: playbook_effects records operation IDs, grant revisions and the command input digest. Reconciliation must find the exact existing row with the exact engine intent. A final receipt can be read back. A pending effect needs current write authority and goes through the same native result owner. A stopped predecessor may have been superseded while its playbook remains open, so do not equate pending decision count with active held tickets.
