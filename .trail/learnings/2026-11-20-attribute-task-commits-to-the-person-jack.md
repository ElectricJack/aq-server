---
title: Attribute task commits to the person who requested the work
author: Jack
date: 2026-11-20
branch: feat/task-git-author
cost: { minutes: 28, tokens: 55806 }
id: l-755d73fc
scope: repo
status: active
src: agent
touches:
  - docs/specs/0019-integrated-delivery.md
  - protocol/dashboard-actions/v1/schema.json
  - crates/aq-delivery/src/commands.rs
  - crates/aq-delivery/src/commands/host.rs
  - crates/aq-delivery/src/github/pulls.rs
  - crates/aq-delivery/src/github/tests/commands.rs
  - crates/aq-delivery/src/github/tests/pulls.rs
  - crates/aq-delivery/src/policy/adapter.rs
  - crates/aq-playbook-domain/src/delivery_runs.rs
  - crates/aq-server/tests/it_playbook/playbook_host9.rs
---

## Decided
Use the authenticated task creator’s AQ account email as the default Git author email. The owner chose this deliberately so an address on the creator’s GitHub account can link the commit and colleagues can find the person who requested the feature. Preserve the worker’s separate committer identity.

## Tried and ruled out
A Graph write can carry a shared service actor even when a human initiated it. Resolving authorship from that field or the current worker loses the requester. A new internal repository ID also cannot distinguish two people filing tasks in the same repository.

## Watch out
A desktop development owner can be a shared local account. Task receipts only establish that account. Never substitute someone’s workstation Git config to guess which person was using it.

In the code: docs/specs/0019-integrated-delivery.md documents attribution. Use the confirmed task-creation journal scoped to the project and Graph workspace.
