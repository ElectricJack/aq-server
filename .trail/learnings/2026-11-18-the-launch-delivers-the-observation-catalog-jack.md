---
title: The launch delivers the observation catalog entry as a manifest module
author: Jack
date: 2026-11-18
branch: feat/observation-sources
cost: { minutes: 593, tokens: 1280475 }
id: l-7a434e0e
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/profiled_execution/producer/build.rs
  - crates/aq-server/src/observations/catalog.rs
---

## Decided

The launch delivers the observation catalog entry as one extra reference in `manifest.modules` (`aq_admission::observation_source`: id `aq.observation-source/v1/<entry digest>/<entry id>`, version = the entry's, digest = SHA-256 of the entry's canonical JSON, which is also the retained Definition body). The module carries the whole entry, so the worker needs no artifact fetch and an older worker ignores it. Source ID and session key are derived in that same file for both sides. The launch-time source is generation 1: the server's source generation, not the assignment generation.

## Tried and ruled out

A new optional field on `NativeOffer` or `OfferedLaunch`: `src/startup.rs`, `src/transport.rs`, `src/control.rs` and their schema artifacts are SHA-256 pinned by the `*.compatibility-sha256.json` inventories, and both structs are `deny_unknown_fields`, so a worker that has not been updated would refuse every offer.

## Watch out

- The catalog's `parsers[].parser` is not a server parser. It must equal the reference the worker's observation engine stamps on events, whose digest is the SHA-256 of the engine's source files (`formats/<f>.rs`, `normalize.rs`, `framing.rs`, `json.rs`, `rows.rs`). An engine edit changes it and the config must follow.
- Delivery alone does not make a batch land. The batch route also needs the node's observation transport owner to be Active on the session's boot (`POST /v1/worker/observations/transport`, Prepare then Activate) and one node-wide contiguous sequence (`first_sequence = through_sequence + 1`). A per-launch store that starts at 1 passes neither on a node that runs several agents.
- `observation_nodes` caps are constants and CHECK constraints: 256 sources per node (never released), 65,536 events and 128 MiB (released 14 days after the task settles).
- aq-server's lib sits at its line ratchet. The identity derivation moved to aq-admission to pay for the producer lines.
