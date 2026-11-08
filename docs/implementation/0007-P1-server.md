# Spec 0007 phase 1: workers as services (aq-server)

Plan: `docs/superpowers/plans/2026-11-07-swarm-p1-workers-as-services.md`
(revision 2), server tasks S0–S13 on branch `swarm/p1-server`. The worker
(aq-worker PRs 4 and 5) and dashboard (aq-dashboard PR 3) halves are in their
own repos.

## What was built

- **S0.** Four modules moved out of the aq-server lib with no behaviour change:
  `aq-row-retention`, `aq-environment-templates`, `aq-provider-quota` and
  `aq-playbook-domain`. This made room under the lib ratchet.
- **S1.** aq-protocol 0.1.1 is vendored (the worker-machine/v1 wire types).
- **S2.** Migration 0101 and the `aq-machine-control` crate: the machine key on
  nodes, the machine's desired state, handoffs, renewed context facts and the
  owner availability switch.
- **S3.** `POST /v1/worker/slots`: slot 1 claims its machine key and mints
  sibling slot nodes.
- **S4.** `POST /v1/worker/control` and `/v1/worker/handoffs`, plus the offer
  gate. A stopped or draining machine gets no new offer, but it still gets its
  cancellations.
- **S5.** The swarm read groups nodes by the server-held machine key and
  reports `control` and each node's service state.
- **S6.** The dashboard actions `worker.start`, `worker.stop`, `worker.drain`
  and `worker.revoke`.
- **S7.** A handoff successor starts from the commit its predecessor saved (an
  aq-git candidate on the predecessor's admitted base). A system task comment
  carries the handoff note.
- **S8.** Owner and service handoffs are not charged to the retry budget, the
  lineage time limit or sandbox minutes (Q1). Crash restarts stay charged.
  aq-selection is now 0.1.5.
- **S9.** A successor that selection would refuse is skipped, and other work is
  offered. A refused poll selection writes no `intelligence_decisions` row (Q1).
- **S10.** The registration receipt cap counts per boot, not per node lifetime.
- **S11.** `allow_enrolled_nodes` on credentials and project bindings (Q3). It
  defaults to on, so any enrolled, non-revoked node may use the credential.
- **S12.** Nodes submit context qualification renewals
  (`POST /v1/worker/qualifications`). Renewals are merged into offer budgeting
  and the swarm read.
- **S13.** Sticky owner availability (Q2): the `swarm.availability.set` action,
  an attestation that selection synthesizes from the switch while it is on,
  and an offer gate while the switch is off. (Fix round 1 replaced the
  original keeper, which rewrote observation rows every 10 s.)

## The machine key model

- One machine identity: the worker-machine/v1 `machine.key`, matching
  `[a-z0-9][a-z0-9-]{0,62}`.
- `aq-worker service enroll` picks the key. Slot 1 redeems the single-use
  code, then claims the key with `/v1/worker/slots` under its own bearer. The
  server stores `nodes.machine_key` and `nodes.slot` for slot 1 and for every
  sibling it mints (slots 2..=8).
- The server-held key is authoritative. Control rows, the offer gate, handoffs
  and revocation are all keyed by it.
- The swarm read groups nodes in this order:
  1. by the server key;
  2. for a legacy node, by its reported key, unless a service machine holds
     that key;
  3. otherwise as `node:<uuid>`.

  `SwarmMachine.id` is the name the actions use.
- One slot is one enrolled node, because aq-server allows one attempt per node.

## Routes

| Route | Auth | Purpose |
| --- | --- | --- |
| `POST /v1/worker/slots` | slot-1 node bearer | Claim the machine key and mint the missing slot nodes |
| `POST /v1/worker/control` | node bearer | The machine's desired state and generation, plus pending handoff requests |
| `POST /v1/worker/handoffs` | node bearer | Record a handoff (`owner_stop`, `service_stop` or `restart`) for a running attempt |
| `POST /v1/worker/qualifications` | node bearer | Submit a context measurement; the server checks it and stores a renewal (128 KiB body limit) |

All four are `node` class in `protocol/commands/v1/routes.json`.

## Tables (migration 0101)

- `nodes.machine_key` and `nodes.slot`: set together; unique per live
  (key, slot).
- `machine_controls`: the desired state per key (`running`, `draining` or
  `stopped`) and a compare-and-swap `generation` (starting at 2; 1 means
  unset).
- `attempt_handoffs`: one row per attempt, with the reason, the saved candidate
  operation and commit, and the note comment.
- `context_qualification_renewals`: renewed facts per renderer digest, with a
  24-hour maximum TTL. Expired rows are deleted when a new renewal is recorded.
- `owner_availability`: a singleton holding the switch and its generation.

## Dashboard actions and capabilities

| Kind | Capability | Notes |
| --- | --- | --- |
| `worker.start`, `worker.stop`, `worker.drain` | `swarm.worker.control@1` | Compare-and-swap on `expected_generation`; `stop` also requests a handoff for every running attempt |
| `worker.revoke` | `swarm.worker.control@1` | Revokes every slot node of the machine; repeating it is confirmed again |
| (enrollment) | `swarm.worker.enroll@1` | Adding a machine |
| `swarm.availability.set` | `swarm.availability.control@1` | Owner only; enabled only when an intelligence registry is configured |

Swarm read fields:
- `SwarmMachine.control` is `{desired, generation, since, pending_handoffs}`,
  or `null` for legacy and `node:` machines;
- each service node's `service.state`;
- `server.owner_availability` is `{state: unset|on|off, generation, since}`.

## The owner answers in code

- **Q1 (handoffs are free).** `owner_stop` and `service_stop` handoffs are
  exempt from the retry budget, the lineage time limit and sandbox minutes.
  `restart` (a crash) is charged.
- **Q1 (no wedged polls).** A refused successor is skipped and other work is
  offered (S9). The pinned-policy pre-check and the workflow backstop both end
  in "no offer this poll", never in a 409.
- **Q2 (sticky availability).** While the switch is on, selection adds a
  synthesized owner `available` attestation, received at the selection's
  `now_ms`, for every authorized credential
  (`aq_intelligence_config::owner_attestations`). No observation row is
  written. A credential with unexpired negative feedback in the node's
  relevant observations gets no attestation, so the feedback still holds it
  off. The offer, preparation, initial-grant and grant-replay fences recompute
  the attestation from the decision's own `now_ms` and the switch's current
  generation, so they refuse only when the owner flipped the switch. While
  the switch is off, polls make no offer and run no selection. When it was
  never set, the old attestation rules apply.
- **Q3 (enrolled nodes are allowed).** `allow_enrolled_nodes` defaults to on.
  Turning it off for a credential changes its digest (a new
  `Placement::Restricted`), and that needs a revision bump.

## Tests

Tests are in the existing binaries:
- `it_access`: `machine_store`, `worker_api`, `intelligence`;
- `it_native`: `native_offers`, `native_registration` and the new
  `native_qualification_renewal`;
- `it_profiled`: the new `replacement_handoff` and `replacement_policy_limits`;
- `it_dashboard`: `dashboard_swarm`, `dashboard_worker_actions`.

There are also crate tests in `aq-machine-control`, `aq-context-renewal`,
`aq-swarm`, `aq-selection` 0.1.5 and `aq-intelligence-config`. The test binary
count stays 81.

## Limits

- **Phase execution health.** The synthesized attestation applies to
  selection and its admission fences. The phase-execution health check still
  reads observation rows only.
- **Sealed history caps.** Every offered attempt writes two
  `intelligence_decisions` rows. One is linked to the attempt's sealed launch
  and is kept forever, by the AQ-08 rule that referenced rows are pinned. The
  table has a global cap of 1,024 rows (`resolver.rs`). Other immutable
  attempt-history tables have lower ceilings:
  - `profiled_launch_registrations`: 256 per node;
  - `profiled_source_exports`: 256 per project;
  - `profiled_launch_closures`: 256 per node, unaccepted.

  An always-on swarm therefore stops offering after a few hundred attempts.
  Pruning only the unlinked decision rows cannot fix this. Plan task S16
  (decision retention) is held for an owner decision on an attempt-history
  maintenance policy.
- **Swarm read.** Running agents are counted from the 128 most recent
  attempts. The count is a stated lower bound when more attempts hold tasks.
- **The handoff successor (S7).** The successor's base is covered at the
  function level; no fixture drives a full replacement poll.

## Owner questions still open

- **Q8 / S16 (sealed history caps):** held for the owner, as above.
- **Q9 (Claude full interactive mode):** goes to the owner only if W13 Step 1
  fails (aq-worker).

The plan's other owner questions are answered or ruled in the ledger.
