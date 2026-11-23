---
title: An online worker needs a verified backend to use a restricted Codex login
author: Jack
date: 2026-11-23
branch: fix/ms1-sol-worker-backend
cost: { minutes: 841, tokens: 3477931 }
id: l-ba0a3f81
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/profiled_execution/presence/inventory.rs
  - crates/aq-server/src/profiled_execution/config.rs
  - crates/aq-server/src/profiled_execution/preparation.rs
---

## Decided

Keep the Codex subscription restricted to Docker-local execution. Carry the daemon operator's backend setting for the exact qualified native runtime through scoped inventory, selection and material preparation. The worker continues to present its existing runtime identity and cannot choose the backend on its inventory request.

## Tried and ruled out

Both Desktop nodes were healthy and polling, with eight idle slots and fresh Codex harness qualifications. Starting the machine again is a no-op when it is already running. Host Codex login, the provider settings reporting present, and a positive subscription usage read do not establish schedulability. The server was omitting the runtime backend from its snapshot and passing no backend through provisioning and acceptance. The login's Docker-local restriction correctly refused an unknown backend.

## Watch out

In the code: look at scoped presence inventory, QualifiedRuntime.backend_kind in profiled_execution configuration, and native preparation. Use a new profiled policy revision with backend_kind = "docker-local" on the exact qualified Desktop Docker runtime. Verify that both fresh snapshots include the restricted credential's preparation binding, then use task work and session detail to prove actual pickup. Retain the configuration revision fence and the credential backend allowlist.
