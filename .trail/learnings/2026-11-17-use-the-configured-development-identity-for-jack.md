---
title: Use the configured development identity for local CLI connections
author: Jack
date: 2026-11-17
branch: feat/aqv2-host-cli-skill
cost: { minutes: 27, tokens: 68695 }
id: l-edac30bc
scope: repo
status: active
src: agent
touches:
  - crates/aqv2/src/lib.rs
  - crates/aq-human-http/src/lib.rs
  - crates/aq-server/src/identity/dev.rs
---

## Decided

Make local HTTP explicit and use the server's configured development identity. The same v2 CLI can reach local and hosted deployments without carrying credentials into the local connection.

## Tried and ruled out

A user-supplied address would be another setting for operators to manage. Repointing the older Python tool would keep its v1 command contract.

## Watch out

Keep development HTTP restricted to loopback, including pinned localhost resolution. Preserve the ordinary Origin and mutation headers so the server can still apply its request checks. Hosted owner sessions use an Access token and sandbox sessions use the authenticated channel.

In the code: `aqv2 --local --origin http://127.0.0.1:28080`, `crates/aqv2/src/lib.rs`, `crates/aq-human-http/src/lib.rs`, and `crates/aq-server/src/identity/dev.rs`.
