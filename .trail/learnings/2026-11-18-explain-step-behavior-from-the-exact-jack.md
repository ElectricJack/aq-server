---
title: Explain step behavior from the exact compiled playbook
author: Jack
date: 2026-11-18
branch: feat/playbooks-step-inspector
id: l-d22c056e
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/step_inspection.rs
  - crates/aq-playbook-domain/src/playbook_view.rs
  - crates/aq-server/src/playbooks/inspection/playbook_view.rs
  - protocol/playbooks-inspection/v1/README.md
---

## Decided
Give a selected step a specific summary, provenance for the fields it uses, and the conditions that move execution to each destination. Keep data links and execution paths visibly distinct. Start with a response projection of the exact retained sealed artifact so existing pins gain useful explanations. The longer-term compiler can persist the same inspection data alongside source mappings without making wording part of the executable fingerprint.

## Tried and ruled out
The routing inspector's list of input names made it reasonable to infer that the model saw only a task identifier. A generic decision description hid the distinction between any route and an owner-set route. The operator and field values behind each branch determine the explanation, so a node's title or execution kind alone is insufficient.

## Watch out
In the code: work.read receives subject and returns facts. The Owned comparison reads task.facts.route and checks for owner. Ask uses the same Read result's title, description, and declared kind only if the workflow reaches Ask. Match guidance to the pinned command contract and check that data links have an unambiguous producer. Project public structure and reviewed enum meanings without prompt text, sensitive literals, or actual run data. The existing stored graph has strict readers, so carry optional inspection metadata alongside it.
