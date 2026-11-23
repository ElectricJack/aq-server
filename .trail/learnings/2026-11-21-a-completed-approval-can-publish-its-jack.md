---
title: A completed approval can publish its unchanged starting commit
author: Jack
date: 2026-11-21
branch: fix/held-task-recovery
cost: { minutes: 1, tokens: 0 }
id: l-e8dca8d4
scope: repo
status: active
src: agent
touches:
  - crates/aq-delivery/src/main_sync/ancestry.rs
---

## Decided
Check the granted base and lease before treating a pushed result as an undelivered change. The owner approval finished, yet its dependent waited for a merge that would never exist. The result had a real central Git push receipt at the original base.

## Tried and ruled out
Activating the draft epic released the Graph parent hold. AQ still refused the offer for delivery ancestry even though Graph showed the prerequisite satisfied. Recreating the edge or adding an empty PR would obscure the original decision.

## Watch out
A retry can inherit unmerged work through its lease. Keep the ancestry check for changed work across attempts. In the code: crates/aq-delivery/src/main_sync/ancestry.rs, undelivered.
