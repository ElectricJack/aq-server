---
title: A worker heartbeat can succeed while capability reports are rejected
author: Jack
date: 2026-11-23
branch: fix/worker-diagnostics
cost: { minutes: 1047, tokens: 4735853 }
id: l-049b183d
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/tests/it_playbook/playbook_host_adapters.rs
  - crates/aq-server/migrations/0172_environment_versions.sql
  - crates/aq-server/src/profiled_execution/definitions.rs
  - crates/aq-server/tests/it_schema/environment_versions_migration.rs
  - crates/aq-server/src/agent_access/human/task_execution.rs
  - crates/aq-server/src/agent_access/mutations/dispatch.rs
  - crates/aq-server/src/profiled_execution/settlement/automatic.rs
  - crates/aq-server/tests/it_playbook/playbook_host9.rs
  - crates/aq-server/tests/it_profiled/profiled_results/auto_settlement56.rs
  - crates/aq-server/tests/it_schema/sealed_history_live.rs
---

## Decided
Show authenticated inventory refusals on Swarm and wait for a fresh accepted report before printing installer readiness. A heartbeat only establishes that a worker can check in.

## Tried and ruled out
Restarting and replacing the worker binaries did not restore placement. The published bundle still carried older harness qualification references than the live server. Registration succeeded while inventory failed. After republishing the qualifications, global overload messages also appeared, but those alone could not be attributed to the laptop.

## Watch out
In the code: crates/aq-server/src/profiled_execution/presence/inventory.rs checks harness references against the live intelligence policy. Verify the hosted native.json against that policy before publishing. crates/aq-server/src/profiled_execution/presence/api.rs correlates authenticated failures by node and request ID. Admission capacity rejections before authentication have no trusted worker identity.
