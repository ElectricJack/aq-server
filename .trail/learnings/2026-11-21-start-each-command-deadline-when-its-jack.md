---
title: Start each command deadline when its transition runs
author: Jack
date: 2026-11-21
branch: fix/held-task-recovery
id: l-f1dd282d
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-server/src/playbooks/driver.rs
---

## Decided
Read the embedding host’s clock between bounded engine transitions. A later command gets its catalog deadline from the time its own intent is created. Recheck current time at dispatch so genuinely expired work still refuses.

## Tried and ruled out
The notification recovery scanner reached the supervisor, but a second cleanly running delivery still lost its first notification. Inspection showed the hold, its host call, the result, and the next notification intent all had one tick timestamp. The next tick arrived 10,572 ms later, beyond the notification’s 10,000 ms deadline. A one-second configured drive interval did not bound the time spent doing work in the preceding tick. Extending the notification timeout would leave other commands exposed and would conceal real expiration.

## Watch out
Preserve the transition limit, durable queue fairness and pinned authority. Inspect both the first dispatch receipt and the fallback mailbox receipt when diagnosing a missed escalation.

In the code: aq-playbook-host executes Request::Tick. The affected server PR was #222, run event-sha256:fd891a2a3e3333a5351446dd5cab732c13d6333a3d9de57f94419f427e43c2db. Delivery receipts 5056 and 5057 show intent and expiry.
