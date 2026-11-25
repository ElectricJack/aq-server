---
title: Keep the assembled offer when PostgreSQL rolls back publication
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
id: l-4b13c1b4
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/profiled_execution/producer/workflow.rs
  - crates/aq-server/src/profiled_execution/producer/publication.rs
---

## Decided
A definite PostgreSQL contention rollback is a retry of the assembled offer. Keep its existing task, slot, assignment identity, source and artifact receipts, and open a new transaction with the original expiry. Recheck machine controls, the current node and the task claim every time.

## Tried and ruled out
After terminal reservations cleared, MS1 assembled new offers faster than publication could commit them. A burst of 163 unique artifact records did not become linked to offers. Every unlinked artifact counted for ten minutes against the project's cap, so normal retries could recreate the capacity wait while idle worker slots were available. Increasing the cap would leave that work and contention intact.

## Watch out
Only a definite rollback can be retried this way. The worker can rediscover a committed offer if the reply was lost. Retain all original expiry, ownership and claim checks. The artifact headroom can still be occupied by assemblies that expire or are definitively refused.

In the code: producer/workflow.rs owns the assembler until publication returns. producer/publication.rs reopens the serializable transaction and performs the full checks. The database lock is 110110 and its first attempt currently times out after two seconds.
