---
title: Show the Markdown that produced the selected playbook version
author: Jack
date: 2026-11-18
branch: feat/playbooks-step-inspector
cost: { minutes: 26, tokens: 24285 }
id: l-e16ee03e
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-authoring/src/runs.rs
  - crates/aq-playbook-authoring/src/source.rs
  - crates/aq-playbook-domain/src/playbook_view.rs
  - crates/aq-server/src/playbooks/inspection/playbook_view.rs
  - crates/aq-server/tests/it_playbook/playbook_activation/definitions.rs
  - protocol/playbooks-inspection/v1/README.md
---

## Decided
The dashboard should show the owner the Markdown that produced the selected compiled artifact. Use the retained saved authoring run for that pin and require registration in the inspected namespace first. Keep compiled step facts separate from the stored graph so older timeline readers and rollback builds can still read it.

## Tried and ruled out
A current settings document can already have edits that have never compiled. The artifact's source digest also differs from the raw Markdown digest even when the text matches because the compiler hashes a generated header and normalized newlines with the document. The routing source map points to anchors shared by several steps, so quoting its mapped line would not reliably explain each node.

## Watch out
In the code: lookup by artifact digest in playbook_authoring_runs, verify the saved Markdown against its raw digest and the normalized SourceBinding, and omit it when the saved document is unavailable. Use names, modes, deadlines, and outcomes from the verified artifact for node details. Omit runtime values and expression bodies.
