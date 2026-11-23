---
title: Retire a routing run only after proving its expired command was never dispatched
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
id: l-a04b7832
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-playbook-host/tests/registered.rs
---

## Decided

For a blocked routing run, retire the expired pending command only when the engine recorded the deadline rejection before any dispatch sequence. A previously dispatched routing mutation may have reached Graph. For locally fenced delivery commands, an already rejected intent can be closed when the host records exact expiry before admission or advanced when the host returns a retained reply.

## Tried and ruled out

The last two blocked records were routing runs left by older command refusals. One had an exact command_deadline_expired receipt, and the other had an operation_rejected receipt from work.read. The engine leaves a blocked run in that state after it records a rejected answer. Treating all rejected routing mutations as harmless would lose the distinction between an unissued command and a lost answer.

## Watch out

Use the exact pending intent and require the recognized expiry reason on a routing command. The engine backfills older dispatch sequences from the first dispatch receipt. A run with evidence of an earlier dispatch remains available for reconciliation. A task's Graph state and its routing run's state are separate.

In the code:
- crates/aq-playbook-host/src/lib.rs routing_expired_before_dispatch guards retirement without a host call.
- crates/aq-playbook-host/tests/registered.rs seeds the two historical intent states and verifies their different outcomes through the owner request.
