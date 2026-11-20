---
title: Keep the merge commit before retrying branch cleanup
author: Jack
date: 2026-11-20
branch: feat/delivery-epic-lifecycle
cost: { minutes: 28, tokens: 21106 }
id: l-8c995907
scope: repo
status: active
src: agent
touches:
  - crates/aq-delivery/src/commands/host.rs
  - crates/aq-server/src/playbooks/host_adapters/delivery_commands.rs
  - crates/aq-server/migrations/0161_delivery_merge_evidence.sql
  - crates/aq-server/tests/it_playbook/playbook_host9.rs
---

## Decided

Keep GitHub's actual merge commit as host evidence keyed by the accepted operation. Branch cleanup can fail after a successful merge. A later driver pass can use the evidence to retry cleanup and the playbook can keep its existing completion contract.

## Tried and ruled out

Adding the SHA to the existing completion changes an immutable catalog. Waiting to record it until cleanup succeeds makes a later retry depend on reading the forge again. The two effects have different commit points.

## Watch out

Historical operations may lack a provider merge SHA. Both the PR head and the current default branch tip are unsafe substitutes for a dependent task or deployment.

In the code: crates/aq-delivery/src/commands/host.rs and crates/aq-server/migrations/0161_delivery_merge_evidence.sql. The host regression is merge_evidence_survives_cleanup_retry. Catalog 2.3.0 returns {} for merged.
