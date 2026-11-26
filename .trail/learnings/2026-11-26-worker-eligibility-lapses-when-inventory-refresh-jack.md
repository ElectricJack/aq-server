---
title: Ready tasks never dispatch when worker disk headroom is short; inventory 503s are a separate, intermittent lock problem
author: Jack
date: 2026-11-26
branch: main
cost: { minutes: 1, tokens: 3133 }
id: l-7617d0ec
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/worker_api.rs
---

## Watch out
"Ready but never dispatched" on 2026-11-26 was a full disk, not the database. Each worker needs 32 GiB free (aq-worker DEFAULT_DISK_LIMIT_BYTES) before it admits a sandbox. With 24.9 GB free it logs `sandbox_disk_headroom_unavailable` in its user journal and offers no environment, so the scheduler records `composition_refused`. Meanwhile the server's capacity view still showed the nodes as eligible with idle slots. Look first with `journalctl --user -u aq-node@ms1` and `df -h /`.

The dashboard provider panel (`credential_unavailable_on_nodes`, 0 nodes) is not evidence: inventory v2 only accepts bindings with available=false, so that panel always reads unavailable under v2.

Separately, inventory refresh returns 503 `row lock contended` some of the time. It takes the `nodes` row FOR UPDATE under a 150 ms lock_timeout, while about 40 other per-node paths take the same lock with no limit. Snapshots still refresh often enough, so this did not stop dispatch, but it adds load. The proposed fix is FOR KEY SHARE for credential checks, a separate node_allocation lock row, and a stored released flag instead of `native_attempt_holds_compute` proof checks; first audit the callers that rely on node-level exclusion.

## Decided
Diagnose order for stalled dispatch: worker journal, then disk, then `performance_scheduling_decisions` reasons for the task, and only then server locks.

In the code: aq-worker crates/aq-worker/src/backend/docker_local/config.rs DEFAULT_DISK_LIMIT_BYTES; aq-sandbox-archive docker_engine/storage.rs; aq-server crates/aq-server/src/profiled_execution/presence/{mod.rs,inventory.rs}; crates/aq-server/src/worker_api.rs lock_current_node; crates/aq-swarm/src/capacity.rs provider_availability.
