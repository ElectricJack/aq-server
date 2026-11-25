---
title: Count the original messages when a failed supervisor turn is retried
author: Jack
date: 2026-11-24
branch: fix/supervisor-held-turn-recovery
id: l-155cc4ab
scope: repo
status: active
src: agent
touches:
  - crates/aq-supervisor-host/src/service/retry.rs
  - crates/aq-supervisor-host/src/service/reservation.rs
  - crates/aq-supervisor-host/src/service/reconcile.rs
  - crates/aq-central-authority/src/reducer/successor.rs
---

## Decided
Count prior dispatches by each retained message's identity and use the most-used message's count. This keeps the retry allowance stable when newer mail joins the next batch or the supervisor controller restarts. A confirmed reply ends the original turn. Automatic release requires proof that the harness never started.

## Tried and ruled out
The full batch's digest changed when more mail arrived, so using it as the retry key would give the old message another allowance. Releasing any stopped reservation after a control change also failed. A stopped handed-off turn can have task effects, and the central successor rejects that same mail even when the outer reservation was canceled.

## Watch out
A missing reply cannot prove there were no changes. Task creation receipts cover only one command path. Preserve the exact stopped dispatch with its mail while action outcome is uncertain. A stop and start may override the retry allowance for a verified nonstart. A lost cancel response must be reconciled with the original reservation and operation.

In the code: crates/aq-supervisor-host/src/service/retry.rs counts exact retained mail membership across generations. service/reservation.rs reuses the cancellation operation and reports the dispatch. The central successor checks retained mail in crates/aq-central-authority/src/reducer/successor.rs.
