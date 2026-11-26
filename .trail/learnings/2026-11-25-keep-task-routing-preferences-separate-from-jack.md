---
title: Keep task routing preferences separate from placement capacity
author: Jack
date: 2026-11-25
branch: fix/placement-capacity-fallback
cost: { minutes: 429, tokens: 961055 }
id: l-33b2debf
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/intelligence/resolver.rs
  - crates/aq-selection/src/candidate_core/ranking.rs
  - crates/aq-swarm-settings/src/placement.rs
---

## Decided
A queued factory task names its intelligence level and may express provider and harness preferences. Placement must try every candidate eligible for that level, even if the task has a historical provider pin or its stored profile has a restrictive fallback reason list. Task-specific edits can demonstrate spare capacity but cannot fix future tasks.

## Tried and ruled out
A fleet snapshot looked like workers were ignoring queued work. The server had recently observed every ready task. Workers kept selecting the preferred provider before checking its zero concurrency limit, so repeated polls reproduced the same wait despite another provider having spare slots. A manual route change started four attempts while leaving the faulty decision process intact.

## Watch out
Checking capacity for each alternative under the shared settings lock repeats fleet occupancy scans. Read one view per placement pass, retain per-candidate reasons, then recheck the selected pool in the serializable offer transaction. A worker heartbeat alone does not establish native placement activity. Use the durable start receipt for launch verification.
In the code: aq-selection candidate ranking, aq-swarm-settings placement, and the native offer publication fence.
