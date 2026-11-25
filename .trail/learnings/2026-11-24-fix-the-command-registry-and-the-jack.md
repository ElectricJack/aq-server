---
title: Fix the command registry and the supervisor mount together
author: Jack
date: 2026-11-24
branch: fix/held-project-evidence
cost: { minutes: 33, tokens: 659919 }
id: l-39314980
scope: repo
status: active
src: agent
touches:
  - crates/aq-command-registry/src/catalog.rs
  - crates/aq-supervisor-control/src/lib.rs
  - crates/aq-supervisor-control/src/read.rs
  - crates/aq-supervisor-host/src/tool.rs
---

## Decided
Keep result and attempt evidence on a scoped GET route in both supervisor modes. Resolve the project from retained AQ storage before using the approving owner's read projection. AQ's authenticated command endpoint and its actual machine router belong in the same regression test.

## Tried and ruled out
Rebuilding the supervisor image alone leaves the held-ticket refusal intact. The tool fetches its command metadata from the server and forwards the command name to the server. The full-access build had routed result and attempt IDs through the owner bridge, whose ordinary URL gate expected a project ID in the path. The separate result-read nest was disabled in that build.

## Watch out
The shared gate must check a profiled attempt against its immutable launch project, including after its task claim is released. Keep the full-access read nest restricted to evidence so mounting it does not broaden access to unrelated owner endpoints. Exercise both feature modes, the approving global supervisor, an allowed project, and a different project using the same retained evidence.

In the code: crates/aq-command-registry/src/catalog.rs, crates/aq-supervisor-control/src/lib.rs, crates/aq-supervisor-control/src/read.rs, crates/aq-supervisor-host/src/tool.rs, crates/aq-server/tests/it_profiled/profiled_results/human_results.rs.
