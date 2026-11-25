---
title: A hosted worker upgrade must ship matching qualification definitions
author: Jack
date: 2026-11-23
branch: deploy/worker-diagnostics
cost: { minutes: 27, tokens: 39071 }
id: l-2b7893dd
scope: repo
status: active
src: agent
touches:
  - crates/aq-dashboard-contract/src/swarm_contract.rs
  - crates/aq-server/src/dashboard_api/swarm.rs
  - crates/aq-server/src/profiled_execution/presence/api.rs
  - crates/aq-swarm/src/inventory_diagnostics.rs
  - crates/aq-server/src/profiled_execution/presence/inventory.rs
  - crates/aq-server/src/profiled_execution/presence/mod.rs
  - crates/aq-server/src/store.rs
  - crates/aq-swarm/src/assemble/nodes.rs
  - protocol/commands/v1/routes.json
  - protocol/dashboard-read/v1/schema.json
---

## Decided
Publish the pinned qualification references with their inventory and composite definition bodies as one checksum-pinned bundle. Use a remote sandbox check to retrieve the worker's own failure stage and verify a new accepted capability report after repair.

## Tried and ruled out
Updating only the native template references moved the failure from a server 409 to a local validation failure. The laptop still checked in and passed sandbox startup, but never submitted a capability report. Its remote check showed native inventory runtime/TTL because the inventory definition still referred to the previous runtime qualification.

## Watch out
In the code: crates/aq-server/src/profiled_execution/presence/qualification.rs matches the report's definition to the registered runtime. On the worker, native.json, inventory.json and both composite definitions need coherent identifiers and digests. Compare against a known working template and the active server policy. The host terminal can repair these files using AQ's systemd user bus; it may lack Windows interop needed by the full installer.
