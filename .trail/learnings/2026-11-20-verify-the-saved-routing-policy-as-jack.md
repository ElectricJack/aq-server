---
title: Verify the saved routing policy as well as the running binding during a catalog upgrade
author: Jack
date: 2026-11-20
branch: rollout/auto-delivery-launch
cost: { minutes: 29, tokens: 2150 }
id: l-efc89426
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-authoring/src/continuity.rs
  - docs/operations/delivery-enrollment.md
---

## Decided
Recompile the last approved routing draft and fixtures against the new host catalog. Promote the authored source and engine binding together. Verify their digests and route a new task before reopening offers.
## Tried and ruled out
Installing the newly sealed routing artifact directly in a fresh namespace succeeded. The engine could execute work.read, but Settings continued to show the old catalog's artifact. A healthy driver and an activated binding by themselves would have allowed an inconsistent live cutover.
## Watch out
A namespace pins its catalog. The source-specific approval can outlive a short operator grant. Reuse the original approved capabilities and check the grant lifetime during a restart test.
In the code: crates/aq-playbook-authoring/src/continuity.rs, docs/operations/delivery-enrollment.md. Compare GET /api/playbooks/bindings/aq.routing and GET /api/settings/routing.
