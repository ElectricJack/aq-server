---
title: Check the current project binding when an old budget hold refuses retry
author: Jack
date: 2026-11-21
branch: fix/held-task-recovery
id: l-297c37b4
scope: repo
status: active
src: agent
touches:
  - crates/aq-human-work/src/lib.rs
  - crates/aq-server/src/agent_access/human/work.rs
  - crates/aq-server/tests/it_native/native_offers/holds.rs
  - crates/aqv2/src/client.rs
  - crates/aqv2/src/lib.rs
  - crates/aqv2/tests/cli.rs
  - crates/aq-server/src/agent_access/human/task_execution.rs
---

## Decided
Before asking for another budget override, compare the binding pinned by the attempt's export with the current project binding. When they differ, use the same retry assessment as the scheduler to explain the effective hold. A fresh retry explicitly starts from the current source.

## Tried and ruled out
After the dashboard's server prerequisites landed, its task still displayed an expired lineage budget. Plain retry returned HTTP 422 and operation lookup returned 404. The owner action rejected the old binding before it admitted an operation. A fresh retry on the current task version was confirmed and a new worker started.

## Watch out
In the code: crates/aq-server/src/agent_access/human/task_execution.rs performs pre-admission checks. crates/aqv2/src/client.rs filters remote error text. Preserve that filter and expose the known binding-change reason with locally defined guidance.
