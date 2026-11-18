---
title: A supervisor that answers nothing is usually a dead Codex login, not a stuck reservation
author: Jack
date: 2026-11-17
branch: fix/supervisor-stranded-reservation
id: l-5c1d0137
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/migrations/0136_stopped_handoff_consumption.sql
  - crates/aq-server/migrations/0137_release_stopped_turn.sql
  - crates/aq-server/src/store/central37.rs
  - "crates/aq-supervisor-host/src/service.rs#stopped_turn"
  - scripts/supervisor-service-manager.py
---

## Decided
- All supervisors share one ChatGPT login file (a volume mounted at /login). A refresh rotates the refresh token, so separate copies lock each other out; one file plus the host's flock on its directory keeps every turn on the newest login.
- A turn that ends without a reply holds its messages; the owner's stop + start of the supervisor retries them. The host never retries by itself.

## Tried and ruled out
- Cancelling the stuck reservation directly: the database refuses to cancel a dispatched reservation (0137 now allows it only after the turn's generation stopped).
- Treating a handed-off, stopped turn as finished (migration 0136): it silently consumed the messages of every turn whose agent crashed. 0137 replaced it; reservations 0136 consumed stay valid proofs.

## Watch out
- `cancellation: "operator"` in a generation's state does not mean a person stopped it: the host records its own stop after "harness pane exited" the same way. Check the pane output first.
- The pane output is gone once the turn ends; capture it live with `tmux -S /state/host/<launch>/tmux.sock capture-pane -p` while a test turn runs. A 401 "refresh token was already used" there means a dead login copy.
- After a WSL crash, supervisors stay stopped (start them by hand once no generation is running) and aq-git needs `recovery clear-fence` for the old incarnation.
- A new migration that redefines a central reservation function must be added to the source chain in `central37.rs` `function_body`, or startup reports the storage as corrupt.

In the code: crates/aq-server/migrations/0136_stopped_handoff_consumption.sql, 0137_release_stopped_turn.sql, crates/aq-server/src/store/central37.rs (function_body), crates/aq-supervisor-host/src/service.rs (stopped_turn), scripts/supervisor-service-manager.py
