---
title: Keep the original approval when a task retry loses its response
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness-main
cost: { minutes: 25, tokens: 12005 }
id: l-b9857247
scope: repo
status: active
src: agent
touches:
  - crates/aq-machine-control/src/owner_retry.rs
  - crates/aq-server/src/agent_access/human/task_execution.rs
  - crates/aq-server/src/dashboard_api/actions/journal.rs
---

## Decided

A new task retry keeps the approved request, project revision, and predecessor in the same transaction as dashboard admission. A bounded recovery sweep rechecks owner permission and the original task version. Release evidence commits with the predecessor's claim release, so recovery can confirm the original operation even if the dashboard response was lost.

## Tried and ruled out

The live journal had two uncertain task retry records that retained the subject, principal, and request digest. Neither record had its original request or an override receipt for its operation ID. A request digest cannot establish the missing reason or fresh-start choice. Later activity on the same task does not prove what the earlier approval did.

## Watch out

Re-executing an approved command against the task's latest attempt can release a successor. Check the frozen predecessor and project binding and use the original operation's receipt. General dashboard actions still need their own evidence before dispatch can be resumed.

In the code:
- Migration 0170 retains new approvals. Older uncertain records require independent evidence or a new owner decision.
- A current replacement release and its recovery receipt commit together. Explicit budget overrides retain their original operation ID.
