---
title: Qualify headless Claude tool access with a loopback model
author: Jack
date: 2026-11-24
branch: feature/claude-supervisor-integrate
cost: { minutes: 30, tokens: 42004 }
id: l-836fcd85
scope: repo
status: active
src: agent
touches:
  - scripts/claude-supervisor-cli-smoke.py
  - crates/aq-supervisor-host/src/subscription/claude.rs
---

## Decided
Use a local Messages API double to qualify the real pinned Claude executable. Make the double request an AQ tool and inspect the following model request for the tool result. This catches differences between a CLI accepting a flag and actually offering the expected tools in unattended mode. A dummy OAuth value suffices.

## Tried and ruled out
Testing against a newer installed executable or reading the help alone misses pin-specific behavior. In 2.1.318 restricted mode works with an explicitly configured stdio MCP server and disabled built-ins. The disable-plugins flag is unavailable. Bare mode changes authentication behavior, so it is a poor proxy for subscription turns.

## Watch out
A successful model reply alone proves little about AQ wiring. Check the action's operation ID against the receipt recorded by the actual supervisor tool process.

In the code: scripts/claude-supervisor-cli-smoke.py, crates/aq-supervisor-host/src/subscription/claude.rs.
