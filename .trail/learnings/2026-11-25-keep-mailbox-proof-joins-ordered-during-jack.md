---
title: Keep mailbox proof joins ordered during retained startup validation
author: Jack
date: 2026-11-25
branch: chore/pin-pgdg-template-worker
cost: { minutes: 29, tokens: 60924 }
id: l-38817efc
scope: repo
status: active
src: agent
touches:
  - "crates/aq-server/src/store/central37.rs#central37_storage_guard"
  - "crates/aq-server/src/store/central37_data.rs#validate"
---

## Decided

Preserve the written join order during central37 retained-data validation. Use transaction-local `join_collapse_limit=1` while checking the retained proofs and restore the prior setting before the remaining startup checks. Keep the frozen predicates and the same transaction. Restored format-178 data took 1,776.865 seconds to start with the previous image. The combined release candidate upgraded restored data to format 183 in 5.686 seconds under the static configuration without PGOPTIONS. A separate restore rehearsal completed in 6.398 seconds. SQLx's prior value of 7 was restored. The same startup path rejected a damaged retained proof and rolled back to format 178 without creating workspace tables.

## Tried and ruled out

PostgreSQL could reorder the mailbox proof joins around the mailbox cursor and reread large message rows before applying the exact message identity. The old startup spent minutes in validation on a restored snapshot with 702 retained reservations. We evaluated disabling JIT and adding a persistent index on isolated copies. Neither was chosen for the release. Preserving the written join order addressed the observed planner choice within the validation transaction.

## Watch out

A row count for reservations hides the repeated member and receipt proof reads. Use a restored production-shaped copy to compare startup plans and check atomic rejection of corrupt evidence. The anonymous retained-proof loop exposes no per-row progress counter. Its apparent delay is insufficient evidence to bypass or narrow any validation.

In the code: `crates/aq-server/src/store/central37.rs` scopes planning in `central37_storage_guard`; `crates/aq-server/src/store/central37_data.rs` retains the source query and exact frozen predicate replay.
