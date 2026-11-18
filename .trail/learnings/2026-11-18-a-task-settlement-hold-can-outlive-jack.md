---
title: A task settlement hold can outlive its worker slot
author: Jack
date: 2026-11-18
branch: fix/finishing-task-settlement
cost: { minutes: 7, tokens: 49452 }
id: l-33d4a388
scope: repo
status: active
src: agent
touches:
  - crates/aq-swarm/src/agents.rs
  - crates/aq-machine-control/src/slots.rs
---

## Decided

Use the scheduler's compute occupancy check for Swarm slots. The live database showed stopped attempts still holding their tasks with stop proof already accepted, including older attempts whose slots had been reused. Showing those task holds as worker activity inflated occupancy and sent owners to disconnected terminals.

## Tried and ruled out

A terminal reconnect cannot resolve the task hold or recover bytes the relay no longer has. Removing task holds on the basis of a stopped process would bypass result and replacement checks that can still be pending after compute is reusable.

## Watch out

A stopped status alone does not prove that the slot is reusable. Continue to show attempts that lack accepted release evidence, so the owner can distinguish a slot that is still finishing from one available for reuse.

In the code:
- The slot-aware Swarm query in `crates/aq-swarm/src/agents.rs` uses the same `native_attempt_holds_compute` SQL function as `crates/aq-machine-control/src/slots.rs`.
