---
title: A confirmed retry can race a scheduler holding the same attempt
author: Jack
date: 2026-11-21
branch: fix/held-task-recovery
id: l-aa2ce4ed
scope: repo
status: active
src: agent
touches:
  - crates/aq-machine-control/src/holds.rs
  - crates/aq-server/src/profiled_execution/lost.rs
  - crates/aq-server/src/profiled_execution/producer/workflow.rs
  - crates/aq-server/src/routing.rs
  - crates/aq-server/tests/it_native/native_offers/holds.rs
---

## Decided
Compare the scheduler's observed retry history before persisting a budget refusal. A supervisor can correctly confirm a retry while a poll is finishing a decision made against the previous budget. The replacement may take another poll to appear.

## Tried and ruled out
Checking only the attempt count allowed the same hold to reappear after the override. On the observed task the next worker started 35 seconds later, after the supervisor had already escalated. Suppressing every future hold on that attempt would hide a genuinely new cost refusal.

## Watch out
In the code: both initial admission and selection of a replacement can record a refusal in crates/aq-machine-control/src/holds.rs. Compare task_limit_overrides with task_holds and execution_attempts before diagnosing a confirmed retry as lost.
