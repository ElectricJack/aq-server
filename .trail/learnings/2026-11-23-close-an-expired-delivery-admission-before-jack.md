---
title: Close an expired delivery admission before announcing the same result again
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
cost: { minutes: 885, tokens: 3471951 }
id: l-8bffe430
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-playbook-domain/src/host_operations.rs
  - crates/aq-server/src/playbooks/host_adapters.rs
---

## Decided

Recover a lost async admission by recording its retained accepted handle in the engine. If the original deadline has passed and no admission exists, record expiry under the same namespace lock the original admission requires. Retire the expired playbook run so the existing result emitter can announce the same ticket result again.

## Tried and ruled out

Looking up more receipt types was insufficient. The publication regression still stalled because engine recovery applied Completed replies and skipped Accepted handles. For the requests that never reached admission, requesting another run immediately could race the original dispatch. The PostgreSQL regression interrupts admission at the operation insert and proves that both writes roll back, the deadline stays open until expiry, and exactly one fresh publication is admitted afterward.

## Watch out

The engine records a rejected reply without making an already blocked run terminal. Resume retirement after a restart if the expiry rejection was already recorded. Keep the expired marker bound to the original intent and grant. A retained admission and an absent admission take different recovery paths.

In the code:
- crates/aq-playbook-host/src/lib.rs reconcile_delivery applies async handles and retires only the run whose exact admission was sealed.
- crates/aq-playbook-domain/src/host_operations.rs close_unadmitted_delivery seals the old intent without creating an external operation.
- crates/aq-server/tests/it_playbook/playbook_host9/delivery/decisions.rs exercises both missing and retained admission replies.
