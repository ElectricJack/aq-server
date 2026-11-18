---
title: Slot counts can time out while PostgreSQL plans stopped-attempt proof checks
author: Jack
date: 2026-11-18
branch: fix/overview-capacity-timeout
cost: { minutes: 7, tokens: 49452 }
id: l-67d91680
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/migrations/0141_plan_compute_stop_checks.sql
  - crates/aq-server/src/store/compute41.rs
  - crates/aq-server/src/dashboard_api/capacity.rs
  - crates/aq-machine-control/src/slots.rs
---

## Decided

Keep the join order fixed inside the existing stop-proof validator. Swarm and Capacity need to count compute occupancy while earlier attempts can still hold their tasks. On the local installation these reads exceeded the 500 ms statement budget with ten held attempts, nine of them finished. A rehearsal clone returned the same result for each attempt after the planner setting, and the occupancy count fell from 324 ms to 37 ms.

## Tried and ruled out

Disabling JIT alone left the count close to the timeout. Loosening the dashboard deadline would hide the read cost. Counting task holds would report released compute as occupied.

## Watch out

The startup guard checks both the validator body and its settings. Accept the exact planner setting from the new schema version onward. Prior schemas still require no function settings.

In the code:
Migration 0141 sets join_collapse_limit=1 on profiled_compute_stopped_valid(uuid,uuid). The compute41 startup guard checks the expected pg_proc.proconfig by state format and still compares the function body.
