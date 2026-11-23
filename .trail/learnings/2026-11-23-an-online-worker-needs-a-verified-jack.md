---
title: An online worker needs a verified backend to use a restricted Codex login
author: Jack
date: 2026-11-23
branch: fix/ms1-sol-worker-backend
cost: { minutes: 864, tokens: 3482785 }
id: l-ba0a3f81
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/profiled_execution/presence/inventory.rs
  - crates/aq-server/src/profiled_execution/config.rs
  - crates/aq-server/src/profiled_execution/preparation.rs
  - crates/aq-server/src/intelligence/login_import.rs
  - crates/aq-server/src/profiled_execution/login_refresh.rs
---

## Decided
Keep the Codex subscription restricted to Docker-local execution. Carry the daemon operator's backend setting for the exact qualified native runtime through scoped inventory, selection and material preparation. The worker continues to present its existing runtime identity and cannot choose the backend on its inventory request.
A granted session and a live Codex process establish task pickup. To diagnose a quiet batch, also check that the sandbox starts a real Codex thread and submits a result. A versioned import of the current same-account login is the supported repair after a confirmed provider authentication failure.

## Tried and ruled out
Both Desktop nodes were healthy and polling, with eight idle slots and fresh Codex harness qualifications. Starting the machine again is a no-op when it is already running. Host Codex login, the provider settings reporting present, and a positive subscription usage read do not establish schedulability. The server was omitting the runtime backend from its snapshot and passing no backend through provisioning and acceptance. The login's Docker-local restriction correctly refused an unknown backend.
The two workers were online, the restricted Codex profile was provisionable, and the host CLI reported signed in. That did not establish that AQ's leased login was valid. Restoring disk headroom allowed new sandbox creation, but the assigned bytes still matched their starts. A bounded read-only query of the selected sandboxes' Codex logs found model discovery HTTP 401s reporting an expired login and zero threads. The host and AQ had the same account with different token expiration times.

## Watch out
In the code: look at scoped presence inventory, QualifiedRuntime.backend_kind in profiled_execution configuration, and native preparation. Use a new profiled policy revision with backend_kind = "docker-local" on the exact qualified Desktop Docker runtime. Verify that both fresh snapshots include the restricted credential's preparation binding, then use task work and session detail to prove actual pickup. Retain the configuration revision fence and the credential backend allowlist.
In the code: queued lease material comes from the server-side login lineage. Compare account and expiry in memory without printing authentication data. Import the current owner login through aq-server import-login with the current credential profile and revision. An operator import ends renewal of old login leases. Observe actual new Codex activity and delivery receipts before reporting successful pickup or an automatic merge.
