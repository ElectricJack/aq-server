---
title: Reconcile stopped work from its recorded receipts before releasing a replacement
author: Jack
date: 2026-11-21
branch: fix/held-task-recovery
cost: { minutes: 417, tokens: 2132660 }
id: l-26a23513
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/delivery_review.rs
  - crates/aq-playbook-domain/src/delivery_store.rs
  - crates/aq-playbook-domain/src/lib.rs
  - docs/implementation/2026-11-21-replacement-effects-unreconciled.md
  - crates/aq-agent-api/src/effects.rs
  - crates/aq-agent-api/src/lib.rs
  - crates/aq-delivery/src/git_grant.rs
  - crates/aq-playbook-domain/src/delivery_decisions.rs
  - crates/aq-profiled-state/src/effect_ledger.rs
  - crates/aq-profiled-state/src/lib.rs
---

## Decided

A stopped delivery attempt can retain its branch and still hold its task for hours because the replacement ledger was never evaluated. Verify the current report, stop proof, retained source and terminal Graph and mailbox receipts together. A completed mailbox send is a side effect that has finished. It can prevent automatic acceptance under a read-only policy and still be compatible with a safe replacement.

## Tried and ruled out

An owner retry of the attachment upload returned replacement_effects_unreconciled even though the central Git branch already held 15 changed files. The live retry did not proceed until AQ reconciled the current failed report. Other attempts had one completed mailbox send and no Graph writes. Treating all mail as unsettled kept those attempts blocked. Ending the claim merely because the worker exited would have skipped checks on unresolved writes.

## Watch out

Check the replacement grant's lease. Its base can still show the older commit while its lease correctly names the predecessor's saved work. For review tasks created before central task branches, use the result's source receipt to locate its verified central checkpoint. A supervisor waking up does not mean it can diagnose a hold: the project, task, attempt and result must travel with the notification.

In the code: crates/aq-server/src/profiled_execution/settlement/automatic.rs, crates/aq-server/src/profiled_execution/settlement/effects.rs and crates/aq-delivery/src/git_grant.rs. The work-detail and operation views distinguish release from actual dispatch.
