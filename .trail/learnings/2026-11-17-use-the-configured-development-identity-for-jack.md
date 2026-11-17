---
title: Use the configured development identity for local CLI connections
author: Jack
date: 2026-11-17
branch: feat/aqv2-host-cli-skill
cost: { minutes: 57, tokens: 117586 }
id: l-edac30bc
scope: repo
status: active
src: agent
touches:
  - crates/aqv2/src/lib.rs
  - crates/aq-human-http/src/lib.rs
  - crates/aq-server/src/identity/dev.rs
  - skills/aq-cli/SKILL.md
  - crates/aqv2/README.md
  - ".github/workflows/release.yml"
---

## Decided
Make local HTTP explicit and use the server's configured development identity. The same v2 CLI can reach local and hosted deployments without carrying credentials into the local connection.
Give the current CLI the familiar command on the host and give the legacy Python tool a versioned name, because both need to be callable on one host and the sandbox already uses the familiar name for v2.

## Tried and ruled out
A user-supplied address would be another setting for operators to manage. Repointing the older Python tool would keep its v1 command contract.
A separate name for the current generation made it easy to reach for the older tool by habit.

## Watch out
Keep development HTTP restricted to loopback, including pinned localhost resolution. Preserve the ordinary Origin and mutation headers so the server can still apply its request checks. Hosted owner sessions use an Access token and sandbox sessions use the authenticated channel.

In the code: `aqv2 --local --origin http://127.0.0.1:28080`, `crates/aqv2/src/lib.rs`, `crates/aq-human-http/src/lib.rs`, and `crates/aq-server/src/identity/dev.rs`.
Keep the published artifact name stable because downstream bundles pin it. Older scripts that still target the legacy daemon must invoke the legacy command explicitly after the switch.

In the code: host v2 `aq`, host v1 `aq1`, published artifact `aqv2-x86_64-linux`. See `skills/aq-cli/SKILL.md`, `crates/aqv2/README.md` and `.github/workflows/release.yml`.
