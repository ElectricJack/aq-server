---
title: A budget hold can follow an already settled result
author: Jack
date: 2026-11-21
branch: integrate/held-task-recovery-current
id: l-883f768c
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/agent_access/human/work.rs
  - crates/aq-playbook-domain/src/scheduling_notices.rs
  - crates/aq-playbook-domain/src/delivery_store.rs
---

## Decided
A project supervisor can act on a budget hold when its own task view carries the worker outcome, the delivery return, the recorded settlement and the retained commit. Match these facts to the current task claim, report, attempt and project. The approved continuation still uses the audited retry command and its existing checks.

## Tried and ruled out
Giving the supervisor the task and result IDs solved the missing-message problem, but its reply still called the result unverified. The task history showed finished attempts and the task view showed the budget hold. Neither showed the recorded return. A direct owner inspection found the failed result already returned for rework with retained source. Asking for another global attempt or results read produced the same scope refusal. The project already has a scoped task read that can explain this without broadening its authority.

## Watch out
A successful browser rerun verifies a particular Git tree. It cannot change the worker's previously recorded outcome or settle an unverified replacement. Distinguish the returned result, the later retry limit, and the current commit when deciding what AQ should do next.

In the code: the project task work projection uses returned_result_evidence. The scheduling notice reads the same evidence from the current claim before notifying its project.
