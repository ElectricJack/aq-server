---
title: A slot that never gets its offer is usually a node row lock upgrade
author: Jack
date: 2026-11-18
branch: fix/ci-green-main
cost: { minutes: 60, tokens: 8050 }
id: l-7cef561d
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/profiled_execution/producer/redelivery.rs
  - crates/aq-server/src/worker_api.rs
  - crates/aq-server/src/execution/authority.rs
  - crates/aq-server/tests/it_native/native_offers/node_slots.rs
---

## Decided

A poll transaction that holds the node row FOR SHARE (worker_api::read_current_node) must not call anything that locks it FOR UPDATE. execution::authority::load_attempt_locked does, through node_revoked. In producer/redelivery.rs existing() two sibling slots with an offer each held the shared lock and asked for the exclusive one; PostgreSQL broke the deadlock after deadlock_timeout (1 s), error::db mapped 40P01 to Busy, and the poll answered an empty page. The loser was the same slot on every poll. existing() now looks under the shared lock and, when it finds an offer, rolls back and repeats under lock_current_node.

## Tried and ruled out

Loading the attempt with Rows::Snapshot inside the shared transaction would drop the upgrade in one line, but nothing shows that every writer of the attempt's dependent rows takes the attempt row lock first. Taking lock_current_node for every poll is what the multi-slot change removed: idle slots queued on the row.

## Watch out

It only shows when the two redelivery transactions overlap. On a 24-core desktop node_slots::eight_slots_of_one_node_poll_at_once passes; with taskset -c 0 about one run in four stalls 36 s or more. The trace is pairs of sqlx slow-statement warnings on 'SELECT revoked_at IS NOT NULL FROM nodes WHERE id=$1 FOR UPDATE' at 1.0 s, one with rows_returned=0, followed by 'background offer assembly refused error=Busy'.
