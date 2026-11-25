---
title: A delivery return can get stuck after a temporary lock error
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
id: l-13ee8aa9
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/profiled_execution/settlement/automatic.rs
  - crates/aq-server/src/playbooks/host_adapters/delivery_decisions.rs
  - crates/aq-playbook-host/src/lib.rs
  - vendor/aq-playbooks-c744301/crates/aq-playbooks-runner/src/lib.rs
---

## Decided
A playbook return is a local settlement of a particular result. Reopen the settlement transaction and recheck the exact report, current head, stop and authority when PostgreSQL reports lock contention. A durable pending return can outlive a server request, so the two-phase owner receipt is the place to reconcile a longer interruption.

## Tried and ruled out
On MS1 three work.return calls reached their pending host rows but reported Busy before a profiled settlement row existed. The grants were still current. The normal tick only tries recovery for qualifying plugin effects, and the generic keyed recovery will not redispatch an expired intent. Waiting for a free worker slot or treating a pending command as a completed return leaves that ticket held.

## Watch out
In the code: query playbook_effects for command_name='work.return' and receipt->>'result_id' before diagnosing result_ready in isolation. Compare the current profiled_result_heads and profiled_auto_settlements. A cancelled or superseded predecessor can leave its own pending operation after its task already moved on. Recover the original authorized decision and preserve the replacement owner's stop, source and effects checks.
