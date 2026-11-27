---
title: Use the remote ref outcome to reconcile a rejected publication
author: Jack
date: 2026-11-25
branch: fix/terminal-task-factory-recovery
cost: { minutes: 30, tokens: 812733 }
id: l-8c4e84ee
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/tests/it_profiled/profiled_results/auto_settlement56/playbook/owner_retry.rs
  - crates/aq-server/src/profiled_execution/settlement/automatic/publication_retry.rs
  - crates/aq-server/tests/it_profiled/profiled_results/auto_settlement56/graph_peer.rs
  - crates/aq-server/src/profiled_execution/producer/workflow.rs
  - crates/aq-server/src/profiled_execution/replacement.rs
  - crates/aq-server/tests/it_native/native_offers/handoff.rs
  - crates/aq-playbook-domain/src/delivery_store.rs
  - crates/aq-server/src/agent_access/human/task_execution.rs
  - crates/aq-server/src/profiled_execution/settlement/automatic.rs
  - crates/aq-delivery/src/git_grant.rs
---

## Decided
Reconcile an owner-requested retry using the terminal Git receipt for the exact ref and result. On this rollout the central task ref was intact, the forge could serve the commit object, and the remote task branch had never been created. A replacement can preserve the central ref and incorporate current main when the effects ledger confirms every old publication is final.

## Tried and ruled out
Repeated publication attempts produced 129 definite rejections while the replacement scheduler kept waiting for an unrecorded delivery decision. The retained result had changed a workflow and the installation intentionally limited its publication credentials. Replaying the same operation or widening the installation permissions would leave the retry handoff unresolved.

## Watch out
A generic transport error can follow a successful push. Keep recovery pinned to the authenticated owner operation and explicit per-ref rejection, and keep incomplete effects and outstanding reviews visible.

In the code: publication retry recovery uses profiled_execution/settlement/automatic/publication_retry.rs and the source refresh checks in aq-delivery/src/git_grant/publication_retry_refresh.sql.
