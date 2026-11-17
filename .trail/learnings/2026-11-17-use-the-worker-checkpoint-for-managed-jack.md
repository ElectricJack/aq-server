---
title: Use the worker checkpoint for managed workspace handoff
author: Jack
date: 2026-11-17
branch: docs/aqv2-workspaces-and-git
cost: { minutes: 27, tokens: 47048 }
id: l-144a0e6a
scope: repo
status: active
src: agent
---

## Decided

Document the managed handoff using the existing result channel because the worker already checkpoints the stopped working tree, including eligible untracked content. Host checkout publishing uses ordinary Git and GitHub when authorized.

## Tried and ruled out

Adding v1-style push commands would give the agent a second path around the project's delivery policy. The live server session list includes profiled attempts whose IDs returned 404 from the direct attempt lookup, so use session detail for the owner inspection example.

## Watch out

A recorded local result is an acknowledgment of the claim. Branch and PR delivery is a later outcome owned by project policy.

In the code: `aq whoami`, `aq result submit`, `aq result status`, `aq session show`, and `skills/aq-workspaces-and-git/SKILL.md`. The worker's `GitAction::Checkpoint` requests mode `all`.
