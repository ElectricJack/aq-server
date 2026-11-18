---
title: Build the all-project Work list inside Graph
author: Jack
date: 2026-11-18
branch: feat/all-project-work
cost: { minutes: 30, tokens: 37753 }
id: l-c91924bb
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/agent_access/human.rs
  - protocol/human-graph/v1/README.md
---

## Decided

Request one global task page from Graph and have it check the shared service credential against each requested workspace in the same query. The recency indexes let the database take at most one page of candidates per workspace before merging those candidates. Searches and lifecycle filters apply before the merge. A local run with 200,000 synthetic tasks across 80 workspaces returned the first 50 in 9.8 ms median and a rare text-and-state search in 12.7 ms median.

## Tried and ruled out

The existing Overview route samples the first few tasks in each project and only resumes at a project boundary. Work needs a task cursor that can continue in the middle of a project. One HTTP request per project also makes response time depend on the slowest workspace reads.

## Watch out

The AQ cursor binds both the AQ project bindings and the Graph credential coverage. If authorization or a project binding changes, ask for a fresh page. Work is ordered by current update time, so a refresh picks up tasks that moved ahead of an existing cursor. Deploy the Graph endpoint and recency indexes before exposing the list through AQ.

In the code:

Graph serves POST /v1/task-list with indexes from migration 0009. AQ serves GET /api/human/work/tasks and binds its cursor in crates/aq-human-work/src/task_list.rs.
