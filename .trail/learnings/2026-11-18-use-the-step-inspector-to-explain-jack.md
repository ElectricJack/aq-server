---
title: Use the step inspector to explain the work in context
author: Jack
date: 2026-11-18
branch: feat/playbooks-step-inspector
cost: { minutes: 29, tokens: 57105 }
id: l-38208f56
scope: repo
status: active
replaces: l-d22c056e
src: agent
touches:
  - crates/aq-playbook-domain/src/step_inspection.rs
  - protocol/playbooks-inspection/v1/README.md
---

## Decided
Opening a node should answer what the selected step does in this playbook and why it is useful. Lead with plain prose derived from the exact compiled artifact. Keep field paths, command names, and bindings under Details so the explanation can be read without expanding technical data. Leave outgoing paths in the graph because it already shows them beside the selected step.

## Tried and ruled out
The earlier inspector listed next steps and their conditions. The owner could already see next steps on the graph and said the duplicate list missed the original request. A summary that only named a command type or recited input keys also left the purpose unclear.

## Watch out
In the code: Read resolves the task and supplies its title, description, and kind to any assessment. Owned checks task.facts.route against owner to preserve a manually chosen route. Ask explains the kind-and-risk assessment and its context when the actual compiled input supports that statement. Keep those explanations tied to the pinned command contract and proven input bindings.
