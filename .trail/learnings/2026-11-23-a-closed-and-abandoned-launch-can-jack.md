---
title: A closed and abandoned launch can keep charging active capacity
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
id: l-f197fece
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/tests/it_native/granted_closure.rs
---

## Decided

Exclude released launches from live history when the immutable owner abandonment names that same launch closure digest. The closure proves execution never reached Started. A still-held task continues to charge capacity through its task claim.

## Tried and ruled out

Deploying staged artifact accounting recovered the charge from refused offer assemblies, yet the AQ Server project still exceeded the admission threshold. Five older attempts had released task claims and a cancelling hot status. A check limited to ordinary stopped receipts missed them. The exact pre-start closure and owner disposition existed for all five and accounted for 111 distinct linked artifacts.

## Watch out

In the code: use profiled_launch_closures and profiled_stopped_dispositions together, with stop_receipt_digest equal to the closure receipt_digest. Keep sealed_attempt_live and sealed_live_attempt_ids consistent. Never infer terminal history from cancelling alone or rewrite a held claim to free quota. The retained artifact bodies and attempt history remain available for evidence.
