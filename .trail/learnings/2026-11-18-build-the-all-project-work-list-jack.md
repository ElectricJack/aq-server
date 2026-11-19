---
title: Build the all-project Work list inside Graph
author: Jack
date: 2026-11-18
branch: feat/all-project-work
cost: { minutes: 34, tokens: 39553 }
id: l-c91924bb
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/agent_access/human.rs
  - protocol/human-graph/v1/README.md
  - protocol/commands/v1/routes.json
  - crates/aq-server/tests/it_server/commands.rs
---

## Decided
Request one global task page from Graph and have it check the shared service credential against each requested workspace in the same query. The recency indexes let the database take at most one page of candidates per workspace before merging those candidates. Searches and lifecycle filters apply before the merge. A local run with 200,000 synthetic tasks across 80 workspaces returned the first 50 in 9.8 ms median and a rare text-and-state search in 12.7 ms median.
Classify the all-project Work read with the existing owner administration entry type. The handler requires a current Owner and checks project bindings and Graph workspace access.

## Tried and ruled out
The existing Overview route samples the first few tasks in each project and only resumes at a project boundary. Work needs a task cursor that can continue in the middle of a project. One HTTP request per project also makes response time depend on the slowest workspace reads.
The focused Human Graph tests passed before the endpoint was listed. Full CI passed the Work pagination and authorization tests and failed the mounted-route inventory check. The inventory entry is sufficient to resolve that failure.

## Watch out
The AQ cursor binds both the AQ project bindings and the Graph credential coverage. If authorization or a project binding changes, ask for a fresh page. Work is ordered by current update time, so a refresh picks up tasks that moved ahead of an existing cursor. Deploy the Graph endpoint and recency indexes before exposing the list through AQ.

In the code:

Graph serves POST /v1/task-list with indexes from migration 0009. AQ serves GET /api/human/work/tasks and binds its cursor in crates/aq-human-work/src/task_list.rs.
Run the mounted-route inventory check alongside the Human Graph tests for a new owner HTTP route. The two checks are in separate test binaries.


GET /api/human/work/tasks is owner_admin in protocol/commands/v1/routes.json. The inventory check is commands::every_mounted_route_is_a_command_or_reviewed in it_server.
