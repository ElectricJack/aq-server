---
title: The original result event owns delivery before the run reads the ticket
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
id: l-4dcc7833
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/delivery_store.rs
  - crates/aq-server/tests/it_playbook/playbook_host9.rs
---

## Decided

Use the last result emission's original durable outbox identity to prevent a second delivery run before the first reaches work.read. The identity can be reconstructed from the persisted task, result, emission time, and facts digest. An acknowledged fanout identifies its exact runs, whose terminal receipts permit a fresh event.

## Tried and ruled out

The live result I traced was emitted twice in sequence after a proved cancellation, so its two events were not evidence of overlap. The prior guard nevertheless had an uncovered interval while an event was queued, fanout was incomplete, or an admitted run had not written work.read. A PostgreSQL regression changes main and pull request facts during those three phases and verifies that all three keep the original event.

## Watch out

An accepted event with a finished fanout and no destinations can be emitted again when facts change or its repeat is due. Older acknowledged events may lack the run list. Preserve the bounded work.read lookup for those records. A new result for the same ticket has its own identity and can be emitted immediately.

In the code:
- crates/aq-playbook-domain/src/delivery_store.rs result_event_id and event_outstanding share the persisted identity used by the emitter.
- crates/aq-server/tests/it_playbook/playbook_host9.rs tests the queued-to-admitted window and legacy fallback.
