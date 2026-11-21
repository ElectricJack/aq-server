---
title: Put repair eligibility in the first delivery notice
author: Jack
date: 2026-11-21
branch: integrate/held-task-recovery-current
cost: { minutes: 1, tokens: 0 }
id: l-2b361fb3
scope: repo
status: active
src: agent
touches:
  - crates/aq-delivery/src/commands.rs
  - crates/aq-delivery/src/github/tests/commands.rs
  - crates/aq-human-work/src/delivery.rs
  - crates/aq-human-work/src/lib.rs
  - crates/aq-playbook-domain/src/delivery_store.rs
  - crates/aq-playbook-domain/src/scheduling_notices.rs
  - crates/aq-server/src/agent_access/human/work.rs
  - crates/aq-server/src/playbooks/host_adapters/assess.rs
  - crates/aq-server/src/playbooks/host_adapters/delivery_commands.rs
  - crates/aq-server/src/playbooks/host_adapters/requests.rs
---

## Decided
Use the same current-result evidence in both the delivery policy's immediate hold notice and the scheduler's fallback notice. For a final PR failure, use the same held-result check as the retry command to tell the project supervisor whether AQ can prepare another attempt.

## Tried and ruled out
The scheduler's enriched notice helped a project supervisor authorize a bounded retry. A different supervisor still stopped after the policy sent a shorter notice first. Both senders intentionally share a deduplication identity, so waiting for the scheduler to send the missing details cannot fix that case. The supervisor tried global result routes, which the project bridge correctly refuses.

## Watch out
GitHub reports a clean branch that needs a base update as behind. The catalog maps this to conflict for rework, so preserve the host observation separately and avoid promising a current retry when a merge is still in flight.

In the code: delivery_decisions::notification, scheduling_notices::notify, delivery_store::held_pr_evidence, git_grant::held_report.
