---
title: Observation caps are a Settings value and sources are released at attempt end
author: Jack
date: 2026-11-18
branch: feat/observation-state-and-caps
cost: { minutes: 527, tokens: 1003284 }
id: l-9af5a674
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/observations.rs
---

## Decided

- The per-node observation caps are the Settings value `observation-limits` (`role_settings.observation_limits`, `GET/PUT /api/settings/observation-limits`, type `aq_role_settings::observation::ObservationLimits`). Enrollment, ingest and usage summaries read it on their own transaction at every use, so a change needs no restart. The CHECK constraints on `observation_nodes` are gone. This replaces the "caps are constants and CHECK constraints" note in the learning about the observation catalog entry.
- A source is released by one `observation_source_releases` row. The row is written by a derived sweep (`aq_row_retention::observation_sources::release_ended`: every source whose attempt has status finished, declined or never_granted), run each stall-ladder tick and by enrollment for its own node before it counts sources. The cap counts sources with neither a release nor a revocation; `observation_nodes.source_count` stays a lifetime counter (transport ownership reads it as "ledger not empty").
- The session read's `observation.state` comes from `aq_stall_ladder::freshness::state` over `last_observed_at_ms` and `last_progress_at_ms` on `observation_attempt_projections`, which ingest maintains. The session list reads them for its whole page in one query.

## Tried and ruled out

- Hooking each attempt-end site to release the source: terminal status is written in more than six places, two in crates other work owns (aq-machine-control lapse, aq-profiled-state replacement). One predicate over the stored status cannot miss a path.
- Reusing `observation_source_revocations` for the release: a revoked source's late usage is recorded as unresolved and never charged, but the final drain of an ended attempt is real usage and must be charged during the grace.
- Deriving the state by scanning `observation_events` per session, as the ladder's evidence query does: the list is polled, and the scan parses every event's JSON.

## Watch out

- aq-server's lib is at its line ratchet. `OwnerSource` moved to `aq_accounting::source` and the source queries to aq-row-retention to pay for this change.
- A batch from a released source after its grace is refused as a whole. The node sequence is gapless, so a worker that keeps retrying that batch stalls its node's later events; the worker must drop events of ended attempts once refused.
- `is_progress` (Rust), `PROGRESS_SQL` and the backfill in the migration are three copies of one rule.
