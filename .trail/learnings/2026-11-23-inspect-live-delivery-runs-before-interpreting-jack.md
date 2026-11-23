---
title: Inspect live delivery runs before interpreting a missing effect receipt
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
id: l-9458540a
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-playbook-domain/src/delivery_decisions.rs
  - crates/aq-server/src/playbooks/driver.rs
---

## Decided

Inspect a waiting result's exact run and effect in the engine's authorized inspection view. Automatic reconciliation of delivery operations can use an exact receipt already recorded by the host. For an unadmitted operation, establish that the original deadline and admission fence have closed before allowing a fresh result event.

## Tried and ruled out

A result had been announced and the emitter continued checking it, but no delivery decision followed. The live PostgreSQL namespace held an encrypted snapshot, so a missing result in the plaintext snapshot and history did not prove the run was gone. Exact engine inspection found four blocked runs on publication or check commands. The host had no effect admission or operation row for those four exact intents. A supported retained-receipt recovery confirmed one different effect and left those four unchanged.

## Watch out

The host commits the acceptance receipt and operation row together before the delivery worker attempts the external effect. A pending engine intent can exist earlier. Preserve the distinction between retrieving a retained answer and authorizing a fresh dispatch. A run in a check join can also be genuinely waiting while an unknown effect in another run requires recovery.

In the code:
- crates/aq-server/src/playbooks/host_adapters/delivery_commands.rs owns durable operation admission.
- playbook.inspection.run exposes the run's current status, pending kind and associated effect IDs.
- crates/aq-playbook-domain/src/delivery_decisions.rs and crates/aq-playbook-host/src/lib.rs implement the targeted retained-receipt sweep.
