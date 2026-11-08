# Swarm phase 1: workers as services — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Revision 2 (2026-11-07).** This revision aligns the plan with swarm phase 2 as merged:
- aq-server #112 (squash `f68684fb`);
- aq-worker #47 (merge `05390a5f`);
- aq-dashboard #85 (merge `9664a5e6`).

It also applies the binding owner answers Q1–Q3 from the ledger (`.superpowers/sdd/2026-11-07-swarm-p1-workers-as-services/progress.md`). "Alignment notes" at the end lists every change against revision 1 and why.

**Goal:** A machine runs `aq-worker` as an always-on Linux/WSL2 systemd user service. Specifically:
- It has several slots that take real docker-local Claude work.
- It is enrolled by an install script and a single-use code, and is allowed automatically.
- The owner can take control of a running docker-local Claude agent's terminal and type into it.
- From the Swarm page it can be started, stopped (save and hand off now), drained and revoked.
- After a restart, interrupted work gets a fresh attempt from what it saved.
- It renews the 24 h context qualification by itself.
- The owner's availability stays on until the owner turns it off.

**Architecture:**
- **One machine identity: the machine key.** It is the same key phase 2 groups machines by, and it is used everywhere:
  - `aq-worker service enroll` picks it: `--machine-key`, or else the lower-cased host name under the rule `aq-worker-ctl machine-config` uses (`[a-z0-9][a-z0-9-]{0,62}`).
  - It writes `machine.json` (the aq-machine-load `MachineConfig` format) into the service's config directory, so load reports carry the key.
  - Slot 1 claims the key on the server through `POST /v1/worker/slots`. The server stores it in `nodes.machine_key` and `nodes.slot`, for slot 1 and for every sibling slot node slot 1 mints.
  - The server-held key is authoritative. Control rows, the offer gate, handoffs and revocation are all keyed by it.
  - The swarm read groups nodes in this order:
    1. by the server-held key;
    2. for a legacy node, by its reported `machine.key`, unless a service machine holds that key;
    3. otherwise as `node:<uuid>`.

    `SwarmMachine.id` is therefore the name dashboard actions use.
  - **Why one key and not revision 1's `machine_id`:** a server UUID per machine would be a second identity that the page never shows. A key that is only self-reported cannot be trusted for control. Claiming the key once, under the slot-1 credential, gives one name that is both visible and authoritative.
- **One slot is one enrolled node.**
  - aq-server allows one attempt per node (`compute::busy_locked`) and one boot per node registration, so a machine with N slots enrolls N nodes that share the key.
  - The first node redeems the single-use code. It then claims the key and mints its sibling slot nodes through the same route.
- **The service drives the existing one-shot path.**
  - Each slot runs the proven `run-native` pipeline: a fresh state directory, a worker-side `aq-git` and `run_once`.
  - `run_once` gains a mode that keeps polling under one registration until an offer arrives.
  - The generic slot supervisor and the OS service layer live in a new crate, `aq-node-service`.
  - The glue to the aq-worker library lives in the `aq-worker` binary. It moves to `src/bin/aq-worker/`, so it is not counted against the lib line ratchet.
- **Control is server-side.**
  - The server stores a desired state per machine key (running, draining or stopped) and gates offers on it.
  - "Stop" also records a handoff request for every attempt running on the machine. The service learns about these from `POST /v1/worker/control` and touches a marker file. The sandbox driver then stops Claude, copies the workspace back and claims a handoff note.
  - The normal result pipeline commits, retains and reports the work as `Failed`. D3 sends it to rework, and B4 replaces it.
  - A new server rule bases a handed-off attempt's successor on the predecessor's saved commit, not on the original base, and posts the handoff note on the task thread.
  - **Owner answer Q1:**
    - An `owner_stop` or `service_stop` handoff is not charged to the B4 budgets or the selection policy's attempt and lineage-time limits.
    - A successor that selection would refuse is skipped in the poll. It never fails the poll, and it never writes a decision row on every poll.
- **Authority (owner answer Q3).** A credential (`[intelligence].credentials[*]`) or project binding with `allow_enrolled_nodes = true` accepts every enrolled, non-revoked node.
  - The flag is set once per credential and once per project, never per machine.
  - Revoking a machine from its Swarm pane revokes all its slot nodes, and that removes the authority.
- **Owner availability (owner answer Q2).** A server keeper keeps the owner's `available` attestation fresh while the owner's switch is on. The switch is on the Swarm page's server pane. "Off" stops new offers.
- **Dashboard.** Every control lives on the phase 2 Swarm page:
  - the machine pane gets Start worker, Stop worker, Drain and Revoke machine;
  - the server pane gets Add machine and Owner availability.

  The phase 2 seam `swarm.worker.control@1` enables the buttons that phase 2 already renders. The old `/api/workers` roster is not extended.

**Tech stack:** Rust 1.96.1 (edition 2024), tokio, axum 0.8, sqlx/PostgreSQL, reqwest (rustls), clap 4, serde/serde_json/toml; systemd user units; bash install script; React 19 + Vitest + MSW (dashboard).

**Spec:** `aq-server/docs/specs/0007-swarm.md` (§5.4, §6, §7.2, §7.3, §8, §10 phase 1). Also read:
- the phase 2 plan, `aq-server/docs/superpowers/plans/2026-11-07-swarm-p2-swarm-page.md`, section "Interfaces with phase 1";
- architecture `docs/architecture/0002-execution-and-recovery.md` §5 and §7, and spec 0006 §5;
- aq-worker `docs/implementation/0006-P3.md` and `docs/operations/install-worker.md` ("Windows host load (WSL2)");
- aq-server `docs/implementation/MS1.md`, sections "Refreshing the context measurement (every 24 hours)" and "Known limits and owners";
- `~/.local/share/aq-ms1/remeasure.sh`;
- aq-server `scripts/ms1-demo.sh`, `scripts/ms1-node-run.py` and `scripts/ms1-profiled.py`.

## Global constraints

Every task's requirements include these.

- **No time estimates** in code comments, docs, commit messages or reports (owner rule).
- **Never use `git stash`.** Commit or discard explicitly. Never edit `/home/jkern/dev/agent-queue2`.
- **Worktrees and target directories.** Each worktree has its own private target directory. Use exactly these:

  | Repo | Worktree | `CARGO_TARGET_DIR` |
  | --- | --- | --- |
  | aq-worker protocol PR | `/home/jkern/dev/aq-platform/wt/swarm-p1-protocol` (branch `swarm/p1-protocol`) | `/home/jkern/dev/aq-platform/target-swarm-p1-protocol` |
  | aq-worker service PR | `/home/jkern/dev/aq-platform/wt/swarm-p1-worker` (branch `swarm/p1-worker`) | `/home/jkern/dev/aq-platform/target-swarm-p1-worker` |
  | aq-server | `/home/jkern/dev/aq-platform/wt/swarm-p1-server` (branch `swarm/p1-server`) | `/home/jkern/dev/aq-platform/target-swarm-p1-server` |
  | aq-worker terminal PR | `/home/jkern/dev/aq-platform/wt/swarm-p1-terminal` (branch `swarm/p1-terminal`) | `/home/jkern/dev/aq-platform/target-swarm-p1-terminal` |
  | aq-server acceptance | `/home/jkern/dev/aq-platform/wt/swarm-p1-acceptance` (branch `swarm/p1-acceptance`) | `/home/jkern/dev/aq-platform/target-swarm-p1-acceptance` |
  | aq-dashboard | `/home/jkern/dev/aq-platform/wt/swarm-p1-dashboard` (branch `swarm/p1-dashboard`) | n/a |

  Create each with `git -C <repo> fetch && git -C <repo> worktree add -b <branch> <path> origin/main`. Never share a target directory. Every `cargo` command below runs with that worktree's `CARGO_TARGET_DIR` exported.
- **Every `cargo test` / `cargo nextest` runs under `timeout`** (`timeout 900 …`). Do not export `CARGO_INCREMENTAL=0`. Do not alternate `cargo build` and `cargo test` without a reason.
- **aq-server integration tests stay in the 8 binaries** (`it_profiled`, `it_native`, `it_phase`, `it_schema`, `it_playbook`, `it_dashboard`, `it_access`, `it_server`).
  - Do not add a `[[test]]` target or a `tests/*.rs` binary.
  - A new test goes into an existing module, or into a new module file listed in that binary's `main.rs`.
  - A test that builds a selection fixture must match the `selection-fixture` filter in `.config/nextest.toml`. `native_offers::` is already in it. Add any new module that uses `common::native_offer::setup` to that filter.
- **aq-worker integration tests go into existing `tests/*.rs` binaries.**
  - Do not add a `tests/*.rs` file. The test binary budget is 105.
  - New crates may have `#[cfg(test)]` unit tests in `src/`; those are not test binaries.
- **Lib line ratchets only go down. There is no ratchet raise** (ledger ruling).
  - aq-server lib: **109,568**, and the lib is at exactly that today. aq-worker lib: **71,389**, also at the ratchet.
  - New code goes in new crates.
  - Each PR's net lib change must be ≤ 0. Tasks S0 and W0 make the room; the "Line budget" table below gives every task's cap.
  - The last task of each PR runs `python3 scripts/build-budget.py --update` and commits the lowered value.
  - Never edit `build-budget.toml` upward.
- **Migrations are contiguous.**
  - The current maximum is `0100` (`0100_swarm_machine_reports.sql`, phase 2).
  - This plan adds exactly one migration, `crates/aq-server/migrations/0101_machine_control.sql`. It ends with `UPDATE meta SET value='101' WHERE key='state_format_version';`.
  - Code that reads a table from 0101 on a store that may be at an earlier migration prefix checks `state_format_version >= 101` first. Follow `replacement::lineage_quarantined` (`profiled_execution/replacement.rs:276-290`), which does this for format 81.
- **Secrets** are never put in argv, in the environment of child processes, in logs or in unit files. This covers the enrollment code, the node credential, the Access id and secret, and the Claude token. Read them from a TTY prompt or stdin lines, or from 0600 files.
- **Plaintext HTTP only to numeric loopback.** A node sends its credential over `http://` only when the host is `127.0.0.1` or `::1`. Any other origin must be `https://`.
- **Contracts.**
  - The aq-server PR changes `protocol/dashboard-read/v1`, through the swarm read and new capabilities, and `protocol/dashboard-actions/v1`, through new action kinds. It merges together with the aq-dashboard re-pin PR, in the same session (owner rule, retrospective W76).
  - `protocol/worker-machine/v1` does not change.
  - aq-protocol 0.1.1 is vendored into aq-server from the aq-worker merge commit.
- **Names.**
  - Phase 2 reserved these names; use them exactly:
    - capability `swarm.worker.control` (version 1);
    - action kinds `worker.start`, `worker.stop` and `worker.drain`;
    - hold codes `worker_stopped` and `worker_draining`;
    - `ServiceState` values `running`, `stopped`, `draining` and `unreported`;
    - the seam `aq_swarm::service::states`.
  - This plan adds:
    - action kinds `worker.revoke` and `swarm.availability.set`;
    - capabilities `swarm.worker.enroll` and `swarm.availability.control`.
  - Phase 2 suggested a `{ node_id }` body for the worker kinds. This plan uses `{ operation_id, machine, expected_generation }`, because control applies to the whole machine and `SwarmMachine.id` is what the machine pane holds.
- **Copy rules.** UI strings are exactly as written in this plan. Never show "phase 1/2/3" to users. Unknown stays "unknown", never 0.
- **The Windows host-load probe is never installed by a script.** The owner has not approved it yet. The installer only prints the instruction from `docs/operations/install-worker.md`.
- **Commit messages** end with:

  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

  PR descriptions end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- **Existing values pinned by this plan:**
  - The B4 limits stay `MAX_ATTEMPTS = 3`, `MAX_LINEAGE_ELAPSED_MS = 24 h` and `MAX_SANDBOX_MS = 240 min` (`profiled_execution/replacement.rs:18-20`). Only what is charged against them changes (S8).
  - The qualification TTL cap stays 86,400,000 ms (`context_budget.rs:125-130`).
  - The heartbeat interval stays 15 s and the offline threshold 60 s. The machine report interval stays 15 s (phase 2).
  - `aq_protocol::MAX_MACHINE_SLOTS = 8`.

## Review Focus

These conditions are not exercised by the happy path. Each line names the test that pins it and the task that owns it.

1. **Stop pressed while an attempt is still preparing.** At that moment no driver directory exists yet. The marker must still be written once the driver directory appears, and the attempt must hand off rather than run to completion.
   - Test: `supervisor::tests::handoff_marker_written_when_driver_dir_appears_later` (W7).
2. **The service is killed between copy-back and report, or salvage runs twice.** Salvage must be idempotent: the second call reports `AlreadyReported` and never posts a second report.
   - Test: `native::runner::recovery::tests::salvage_is_idempotent_after_report` (W4).
3. **Stop pressed twice, or Start pressed while a handoff is in flight.** The generation compare-and-swap refuses a stale `expected_generation`. Handoff rows are not duplicated. Start never cancels a handoff that was already requested.
   - Tests: `worker_api::machine_control_generation_conflicts_and_idempotent_handoffs` (S4) and `dashboard_worker_actions::stale_generation_is_a_conflict` (S6).
4. **Qualification renewal while slots are polling** (the renderer switch). No offer is taken with a stale renderer pin.
   - Test: `supervisor::tests::renderer_switch_pauses_polling_in_quiet_window` (W7).
5. **Every poll is refused**, for example with 409 for an expired qualification or a held registration.
   - The slot must back off exponentially (30 s up to 300 s) and must not hot-loop or burn state directories.
   - The server must log the qualification reason at `warn`.
   - Tests: `supervisor::tests::failing_slot_backs_off_and_prunes_empty_dirs` (W7) and `native_qualification_renewal::expired_fact_logs_warn_reason` (S12).
6. **The owner stops the same task's machine again and again (Q1).** Owner and service handoffs never exhaust the retry, lineage-time or sandbox-minute budget. A crash restart (`restart`) is still charged, so a crash loop stays bounded.
   - Tests: `replacement::tests::owner_and_service_handoffs_are_exempt_restart_counts` (S8) and aq-selection `exempt_attempts_do_not_count_against_policy_max_attempts` (S8).
7. **Selection would refuse a successor (Q1).**
   - The task is skipped and other ready work is still offered.
   - The poll never answers 409 because of that task.
   - A refused poll selection writes no `intelligence_decisions` row. The table is capped at 1,024 rows and nothing prunes it.
   - Tests: `replacement_policy_limits::pinned_policy_budget_blocks_before_selection` (S9) and `native_offers::refused_poll_selection_records_no_decision` (S9).
8. **A machine is revoked from its pane (Q3).**
   - Every slot node is revoked.
   - No new slot can be minted under the key until a fresh enrollment claims it.
   - The service stops polling instead of looping.
   - Tests: `machine_store::revoke_machine_revokes_every_slot_and_clears_control` (S2), `worker_api::slots_refused_after_machine_revoked` (S3) and `supervisor::tests::revoked_credential_pauses_all_slots` (W7).
9. **The owner turns availability off (Q2).** No new offer is made and no decision row is written. Turning it on again restores offers without a manual observation.
   - Tests: `native_offers::owner_availability_off_makes_no_offer_and_no_decision` (S13) and `intelligence::keeper_refreshes_owner_available_observation` (S13).

10. **Typed input reaches only an interactive Claude attempt, and never carries the token.** The default recipe still refuses input, and a typed line holding the token is never uploaded.
   - Tests: `claude::tests::default_recipe_still_refuses_input` and `sandbox_driver::tests::input_holding_the_token_is_refused_and_never_uploaded` (W13).

---

## Line budget

`python3 scripts/build-budget.py` counts `.rs` lines in each crate's `src/`, minus bin targets.

**aq-server lib: 109,568 today, ratchet 109,568.**

| Task | Change to `crates/aq-server/src` | Lines |
| --- | --- | --- |
| S0 | Four moves into new crates: row retention (−437), environment templates (−243), provider quota (−584), playbook domain (−334) | **≈ −1,598**. After S0 the count must be **≤ 108,000** |
| S3 | `machine_api.rs` ≤ 120, plus one line each in `worker_api.rs` and `lib.rs` | ≤ +122 |
| S4 | `machine_api.rs` ≤ +60; offer gate in `workflow.rs` ≤ +8; `context_renewal.rs` stub ≤ 6; one line in `profiled_execution.rs` | ≤ +75 |
| S5 | `dashboard_api/swarm.rs` | ≤ +10 |
| S6 | `machine_actions.rs` ≤ 200; `existing.rs` +2; `service.rs` +2; `lib.rs` +1 | ≤ +205 |
| S7 | `handoff.rs` ≤ 140; `source.rs` ≤ +40; `workflow.rs` ≤ +30; one line in `profiled_execution.rs` | ≤ +211 |
| S8 | `replacement.rs` ≤ +32; `resolver.rs` ≤ +4; `workflow.rs` ≤ +2; `wakes.rs` ≤ +7 | ≤ +45 |
| S9 | `continuation_policy.rs` ≤ +30; `replacement.rs` ≤ +14; `workflow.rs` / `build.rs` ≤ +16; `resolver.rs` ≤ +10 | ≤ +70 |
| S10 | `registration.rs` | ≤ +1 |
| S11 | 11 one-line replacements (net 0); `registry.rs` ≤ +6; `project_bindings/domain.rs` ≤ +6; `notifications/sources.rs` 0 | ≤ +12 |
| S12 | `context_renewal.rs` ≤ 150 (replaces the stub); `machine_api.rs` ≤ +20; `workflow.rs` / `build.rs` ≤ +12; `swarm.rs` ≤ +5 | ≤ +181 |
| S15 (PR 6) | `producer/result_startup.rs` | ≤ +15 |
| S13 | `intelligence/keeper.rs` ≤ 70; `intelligence/availability_action.rs` ≤ 120; `intelligence.rs` ≤ +20; `lib.rs` +1; `workflow.rs` ≤ +8; `swarm.rs` ≤ +4; `existing.rs` +2; `service.rs` +1 | ≤ +226 |
| **Total** | | **≈ −405 net** |

At the end of each task, run `python3 scripts/build-budget.py`. If the aq-server count is above `108,000 + (sum of the caps of the tasks done so far after S0)`, cut that task's lines, for example by moving pure logic into `aq-machine-control`, before committing.

**aq-worker lib: 71,389 today, ratchet 71,389.**

| Task | Change to `crates/aq-worker/src` (lib) | Lines |
| --- | --- | --- |
| W0 | Bin-only modules move under `src/bin/aq-worker/` (101 + 339 + 231 + 235 lines) | −906 → **70,483** |
| W2 | `native/runner.rs`, `native/runner/recovery.rs` | ≤ +90 |
| W3 | `sandbox_driver.rs` | ≤ +130 |
| W4 | `native/runner/recovery.rs`, `native/journal.rs` | ≤ +220 |
| W5 | `native/runner/claude_docker.rs` | ≤ +10 |
| W13 (PR 5) | `native/adapter/claude.rs`, `native/adapter/result_profile.rs`, `sandbox_driver.rs` | ≤ +200 |
| **Total** | | **≈ −256 net** |

`aq-node-service`, `aq-machine-load`, `aq-terminal-bridge` and the `aq-worker` binary are separate counts. Each must stay under the 40,000 target.

---

## File structure

### aq-worker (`/home/jkern/dev/aq-platform/aq-worker`, read from `origin/main`)

| Path | Status | Responsibility |
| --- | --- | --- |
| `crates/aq-protocol/src/lib.rs` | modify (W1) | Machine wire types: slots with the machine key, control, handoffs, qualification renewal |
| `crates/aq-protocol/fixtures/machine-v1.json` | create (W1) | Golden JSON for every new type; copied into aq-server |
| `crates/aq-worker/src/bin/aq-worker/main.rs` (moved from `src/main.rs`) | move + modify (W0, W9) | CLI: `run --service-config` and the `service …` subcommands |
| `crates/aq-worker/src/bin/aq-worker/{native_diagnostics.rs,native_fixture.rs,native_fixture/replay.rs,fixture_responses_cli.rs}` | move (W0) | Bin-only modules, now outside the lib count |
| `crates/aq-worker/src/bin/aq-worker/service.rs` | create (W9) | `NativeSlotBackend`, the aq-worker lib glue for `aq-node-service`; `machine.json`; machine reporting |
| `crates/aq-worker/src/bin/aq-worker/measure.rs` | create (W10) | Docker-local measurement run (the `remeasure.sh` procedure) |
| `crates/aq-worker/src/native/runner.rs` | modify (W2) | `run_until_attempt`, `poll_until_offer` |
| `crates/aq-worker/src/native/runner/recovery.rs` | modify (W2, W4) | `retained_attempts`, `salvage_interrupted` |
| `crates/aq-worker/src/sandbox_driver.rs` | modify (W3) | Handoff marker, copy-back, handoff note claim |
| `crates/aq-worker/src/native/runner/claude_docker.rs` | modify (W5) | Allow an Access-configured enrollment |
| `crates/aq-terminal-bridge/src/http.rs` | modify (W5) | `RelayHttp::with_access` |
| `crates/aq-node-service/**` | create (W6–W8, W10) | Service config, layout, server link, aq-git child, slot supervisor, OS service layer, measurement prompts |
| `packaging/service/aq-install.sh`, `packaging/service/tests/test_install.py` | create (W11) | Install script |
| `crates/aq-worker/src/native/adapter/{claude.rs,result_profile.rs}`, `crates/aq-worker/src/sandbox_driver.rs` | modify (W13) | Interactive docker-local Claude recipe; typed input as streaming user messages |
| `packaging/measure/{minimal,mixed,adversarial}.txt` | create (W10) | Measurement prompts |
| `docs/operations/install-service.md` | create (W11) | Operator guide |

### aq-server (`/home/jkern/dev/aq-platform/aq-server`)

| Path | Status | Responsibility |
| --- | --- | --- |
| `crates/aq-row-retention/**`, `crates/aq-environment-templates/**`, `crates/aq-provider-quota/**`, `crates/aq-playbook-domain/**` | create (S0) | Code moved out of the aq-server lib to pay for this phase's lines |
| `vendor/aq-protocol-0.1.1/**` | create (S1) | Vendored machine wire types |
| `crates/aq-server/migrations/0101_machine_control.sql` | create (S2) | `nodes.machine_key/slot`, `machine_controls`, `attempt_handoffs`, `context_qualification_renewals`, `owner_availability` |
| `crates/aq-machine-control/**` | create (S2) | SQL store and rules for machines, controls, handoffs, revocation and owner availability (no `AppState`) |
| `crates/aq-server/src/machine_api.rs` | create (S3, S4, S12) | `/v1/worker/slots`, `/v1/worker/control`, `/v1/worker/handoffs`, `/v1/worker/qualifications` |
| `crates/aq-swarm/src/{service.rs,assemble.rs}`, `crates/aq-dashboard-contract/src/swarm_contract.rs`, `crates/aq-server/src/dashboard_api/swarm.rs` | modify (S5, S12, S13) | Fill the phase 2 seam: service states, machine control, server-key grouping, owner availability |
| `crates/aq-server/src/machine_actions.rs` | create (S6) | Dashboard action kinds `worker.start` / `worker.stop` / `worker.drain` / `worker.revoke` |
| `crates/aq-server/src/profiled_execution/handoff.rs` | create (S7) | Successor base from the handoff commit; handoff note comment |
| `crates/aq-server/src/profiled_execution/replacement.rs`, `crates/aq-selection/src/{domain.rs,select.rs}` | modify (S8, S9) | Q1 budget exemption and the pinned-policy pre-check |
| `crates/aq-intelligence-config/src/lib.rs`, `crates/aq-server/src/project_bindings/domain.rs` | modify (S11) | Q3 `allow_enrolled_nodes` |
| `crates/aq-context-renewal/**` | create (S12) | Pure measurement validation and fact renewal |
| `crates/aq-server/src/profiled_execution/context_renewal.rs` | create (S4 stub, S12) | `current_facts`, `current_pin`, renewal storage |
| `crates/aq-server/src/intelligence/{keeper.rs,availability_action.rs}` | create (S13) | Q2 sticky owner availability and the `swarm.availability.set` action |
| `crates/aq-server/src/profiled_execution/producer/result_startup.rs` | modify (S15) | Seal `input_enabled` for the interactive Claude profile |
| `scripts/ms1-profiled.py`, `scripts/ms1-demo.sh` | modify (L0) | Acceptance flags and commands |

### aq-dashboard (`/home/jkern/dev/aq-platform/aq-dashboard`)

| Path | Status | Responsibility |
| --- | --- | --- |
| `contracts/pinned/dashboard-read/**`, `contracts/pinned/dashboard-actions/v1/**`, `contracts/live/dashboard-actions/**`, `contracts/pinned/source.json` | modify (D1) | Re-pin |
| `src/api/wire/swarm.ts`, `src/api/adapters/swarm.ts` (+ `.test.ts`), `src/mocks/swarm.ts` | modify (D2) | Machine control and owner availability in the swarm view |
| `src/api/adapters/projectBindings.ts` (+ `.test.ts`), `src/features/settings/ProjectBindings.tsx` | modify (D2) | `allow_enrolled_nodes` |
| `src/features/agents/swarm/{swarmActions.ts,SwarmAction.tsx,MachineControls.tsx}` (+ tests), `SwarmPane.tsx`, `src/mocks/swarmActions.ts` | create/modify (D3) | Start / Stop / Drain / Revoke on the machine pane |
| `src/features/agents/swarm/AddMachine.tsx` (+ test), `src/features/settings/FrameworkStatus.tsx` | create/modify (D4) | Enrollment code and install command |
| `src/features/agents/swarm/OwnerAvailability.tsx` (+ test) | create (D5) | The availability switch on the server pane |

---

## Repo and merge order

Merges that touch the same subsystem go one at a time, with main CI green between them. Each repo merges through one PR per row. Rows merge strictly in this order, and tasks inside a PR run in the listed order. Tasks inside one PR that touch the same subsystem are sequential, never parallel.

**Preconditions (state of main on 2026-11-07):**
- Phase 2 is fully merged.
- Phase 2's A1 live acceptance was dispatched on the aq-ms1 stack. Phase 1's live acceptance (L) uses the same stack, so L starts only after A1 has reported.
- aq-dashboard `fix/routeerror-flake` (the `RouteError.test` flake fix) is in flight. PR 3 merges only after it has merged or been closed, and only with dashboard main CI green.

| # | PR | Tasks | Merges after |
| --- | --- | --- | --- |
| 1 | aq-worker `swarm/p1-protocol` | W1 | aq-worker main CI green on `05390a5f` or later |
| 2 | aq-server `swarm/p1-server` | S0 → S1 → S2 → S3 → S4 → S5 → S6 → S7 → S8 → S9 → S10 → S11 → S12 → S13 → S14 | PR 1 merged (S1 vendors its merge commit); aq-server main CI green |
| 3 | aq-dashboard `swarm/p1-dashboard` | D1 → D2 → D3 → D4 → D5 | Opened beside PR 2. **Merged in the same session as PR 2**: merge PR 2, wait for aq-server main CI green, re-pin D1 to PR 2's merge commit, wait for PR 3 CI green, then merge PR 3 |
| 4 | aq-worker `swarm/p1-worker` | W0 → W2 → W3 → W4 → W5 → W6 → W7 → W8 → W9 → W10 → W11 → W12 | PRs 1 and 2 merged (the service calls the new server routes); aq-worker main CI green |
| 5 | aq-worker `swarm/p1-terminal` | W13 | PR 4 merged (both change `sandbox_driver.rs`, the same subsystem); aq-worker main CI green |
| 6 | aq-server `swarm/p1-acceptance` | S15 → L0, then L (live) | PRs 2–5 merged (S15 pins PR 5's recipe digest); aq-server main CI green; A1 reported |

Parallelism allowed:
- PR 3's D2–D5 may be developed while PR 2 is in progress, against S5, S6 and S13's planned contracts.
- PR 4's W0 and W2–W5 may be developed while PR 2 is in review.
- PR 5's W13 Step 1 (the probe) may run any time; its code starts after PR 4 merges.

Nothing else runs in parallel.

---

## Task list

### W1: aq-protocol 0.1.1 machine wire types (aq-worker, PR 1) — Sonnet

**Files:**
- Modify: `crates/aq-protocol/Cargo.toml`. Set the version to `0.1.1` and add `serde_json.workspace = true` to `[dependencies]`.
- Modify: the root `Cargo.toml` (workspace `aq-protocol = { path = "crates/aq-protocol", version = "=0.1.1" }`).
- Modify: `crates/aq-protocol/src/lib.rs`
- Create: `crates/aq-protocol/fixtures/machine-v1.json`

**Interfaces — Produces.** All types derive `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]` and all structs are `#[serde(deny_unknown_fields)]`, except where noted.

```rust
pub const MAX_MACHINE_SLOTS: u32 = 8;
pub const MACHINE_SCHEMA_VERSION: u32 = 1;

/// The worker-machine/v1 `machine.key` rule: 1-63 characters of [a-z0-9-], not starting with '-'.
pub fn valid_machine_key(key: &str) -> bool;

/// POST /v1/worker/slots, under the bearer of the machine's slot-1 node.
/// Claims `machine_key` for the caller (slot 1) and mints the missing slots 2..=slots.
pub struct SlotsRequest { pub schema_version: u32, pub machine_key: String, pub slots: u32 }   // slots: total wanted, 1..=8
pub struct SlotsResponse { pub schema_version: u32, pub machine_key: String, pub slots: Vec<SlotEnrollment> }
// Debug is implemented by hand and prints credential as "<redacted>".
pub struct SlotEnrollment {
    pub slot: u32,                     // 2..=8 (slot 1 is the caller)
    pub node_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,    // Some only in the response that minted it
}

#[derive(Copy)] #[serde(rename_all = "snake_case")]
pub enum DesiredState { Running, Draining, Stopped }

/// POST /v1/worker/control, under any slot node's bearer.
pub struct ControlRequest { pub schema_version: u32 }
pub struct ControlResponse {
    pub schema_version: u32,
    pub machine_key: String,
    pub desired: DesiredState,
    pub generation: u64,               // >= 1
    pub handoff: Vec<Uuid>,            // attempts on this machine to hand off now
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualification: Option<QualificationPin>,
}
pub struct ArtifactPin { pub id: String, pub version: String, pub digest: String } // digest: 64 lowercase hex
pub struct QualificationPin { pub renderer: ArtifactPin, pub effective_at_ms: u64, pub expires_at_ms: u64 }

#[derive(Copy)] #[serde(rename_all = "snake_case")]
pub enum HandoffReason { OwnerStop, ServiceStop, Restart }
/// POST /v1/worker/handoffs (node announces a handoff it is performing).
pub struct HandoffNotice { pub schema_version: u32, pub attempt_id: Uuid, pub reason: HandoffReason }

/// POST /v1/worker/qualifications. `result` is the Claude stream-json `result` event, verbatim.
pub struct MeasuredCall { pub call: String, pub prompt_utf8_bytes: u64, pub prompt_sha256: String, pub result: serde_json::Value }
pub struct MeasurementSubmission { pub schema_version: u32, pub calls: Vec<MeasuredCall> }
// Response body is QualificationPin.
```

Do not change any existing type.

- [ ] **Step 1: Write the failing tests** in the existing `#[cfg(test)] mod tests` of `crates/aq-protocol/src/lib.rs`:
  - `machine_fixture_round_trips`:
    - `include_str!("../fixtures/machine-v1.json")` is a JSON object with the keys `slots_request`, `slots_response`, `control_request`, `control_response`, `handoff_notice`, `measurement_submission` and `qualification_pin`.
    - Each value deserializes into its type and re-serializes to an equal `serde_json::Value`.
  - `slot_enrollment_debug_redacts_credential`: `format!("{:?}", SlotEnrollment{slot:2,node_id:Uuid::nil(),credential:Some("aqn_secret".into())})` contains `"<redacted>"` and does not contain `aqn_secret`.
  - `desired_state_is_snake_case`: `serde_json::to_string(&DesiredState::Draining).unwrap() == "\"draining\""`, and `HandoffReason::ServiceStop` serializes to `"service_stop"`.
  - `unknown_fields_are_refused`: `serde_json::from_str::<ControlRequest>(r#"{"schema_version":1,"x":1}"#)` is `Err`.
  - `machine_key_rule_matches_worker_machine_v1`:
    - `valid_machine_key` is true for `"desktop"`, `"a"`, `"desk-1"`, `"0a"` and `"a".repeat(63)`;
    - it is false for `""`, `"-a"`, `"Desk"`, `"a_b"`, `"a b"` and `"a".repeat(64)`.
- [ ] **Step 2: Write the fixture** `crates/aq-protocol/fixtures/machine-v1.json`, with one example per key above:
  - `slots_request = {"schema_version":1,"machine_key":"desktop","slots":3}`;
  - `slots_response` has `machine_key` `"desktop"` and two slots, one with a credential and one without;
  - `control_response` has `machine_key` `"desktop"`, `desired` `"stopped"`, `generation` 2, one handoff UUID, and a `qualification` with digest `"a"` repeated 64 times.
- [ ] **Step 3: Run and see it fail.** Run `cd /home/jkern/dev/aq-platform/wt/swarm-p1-protocol && CARGO_TARGET_DIR=/home/jkern/dev/aq-platform/target-swarm-p1-protocol timeout 900 cargo test -p aq-protocol --locked`. Expected: compile errors for the missing types.
- [ ] **Step 4: Implement the types** exactly as listed. Implement `valid_machine_key` as:

  ```rust
  (1..=63).contains(&key.len())
      && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
      && !key.starts_with('-')
  ```

  This is the same rule as aq-server's `aq_swarm::machine::MachineReport::validate` and aq-worker's `aq_machine_load::MachineConfig::load_optional`.
- [ ] **Step 5: Run it again.** Run the same command, then `cargo test --workspace --locked --all-features --no-run` under `timeout 900`, so that every crate still builds against 0.1.1. Expected: PASS, and `python3 scripts/build-budget.py` prints OK.
- [ ] **Step 6: Commit and open PR 1.** Commit message: `feat(protocol): aq-protocol 0.1.1 machine wire types (spec 0007 phase 1)`.

---

### S0: Make room in the aq-server lib (aq-server, PR 2) — Sonnet

Move four self-contained modules into new crates, with no behaviour change. Commit each part separately. Do **not** run `--update` (S14 does).

**General rules for every part:**
- Cut and paste exactly. Keep every SQL string and every test byte-identical.
- In the moved file, change `pub(crate)` and `pub(super)` to `pub`.
- Copy each dependency line exactly as `crates/aq-server/Cargo.toml` declares it. Use `workspace = true` where aq-server does.
- Add each crate to the root `Cargo.toml` `members`, and as a path dependency in `crates/aq-server/Cargo.toml`:
  - `aq-row-retention = { path = "../aq-row-retention" }`
  - `aq-environment-templates = { path = "../aq-environment-templates" }`
  - `aq-provider-quota = { path = "../aq-provider-quota" }`
  - `aq-playbook-domain = { path = "../aq-playbook-domain" }`
- Each `Cargo.toml` has `version = "0.1.0"`, `edition.workspace = true`, `publish = false` and a one-line `description`, as `crates/aq-swarm/Cargo.toml` does.
- If the compiler reports an orphan-rule or inherent-impl error for a moved type, move that `impl` block into the new crate too.

**Part A — row retention.**
- Create `crates/aq-row-retention/{Cargo.toml,src/lib.rs}`. Its deps are `anyhow`, `async-trait`, `sqlx`, `sqlx-postgres` and `uuid`.
- `crates/aq-server/src/row_retention.rs` becomes a shim with:
  - the module doc;
  - `pub use aq_row_retention::*;`
  - the unchanged `pub async fn run(state: crate::AppState, mut stop: tokio::sync::watch::Receiver<bool>)` (lines 443-460).
- The moved `TaskState` gains a blanket `impl<T: TaskState + ?Sized> TaskState for Option<std::sync::Arc<T>>`, which returns `Ok(false)` for `None`. It replaces aq-server's `impl TaskState for Option<Arc<crate::agent_access::Runtime>>` (`row_retention.rs:18-26`), which the orphan rule would forbid after the move. `agent_access/graph.rs:493` keeps `impl crate::row_retention::TaskState for Runtime`.
- Every `#[cfg(test)]` in `row_retention.rs` moves.
- Add the test `option_arc_of_task_state_delegates_and_none_is_incomplete` to `crates/aq-row-retention/src/lib.rs`:
  - a struct `Always(bool)` implements `TaskState`;
  - `Some(Arc::new(Always(true))).completed(..)` returns `Ok(true)`;
  - `None::<Arc<Always>>` returns `Ok(false)`.
- Commit message: `refactor: move row retention into aq-row-retention (room for spec 0007 phase 1)`.

**Part B — environment templates.**
- Create `crates/aq-environment-templates/{Cargo.toml,src/lib.rs}`. Its deps are `aq-admission`, `aq-profiles`, `serde`, `sqlx`, `sqlx-postgres` and `thiserror`.
- The crate defines `pub enum Error { #[error("database")] Database }` (`thiserror`). The moved `store`/`load` return it.
- `crates/aq-server/src/templates.rs` becomes a shim with:
  - `pub use aq_environment_templates::{Allowlist, Resolution, resolve, catalog_environments, deserialize};`
  - two wrappers: `pub async fn store(tx: &mut sqlx::PgConnection, catalog: &Digest, templates: &[Resolution]) -> Result<(), AppError>` and `pub async fn load(conn: &mut sqlx::PgConnection, catalog_digest: &Digest) -> Result<Vec<Resolution>, AppError>`. Each maps `aq_environment_templates::Error` to `AppError::Internal`.
- The tests module at `templates.rs:203` moves unchanged.
- Commit message: `refactor: move environment templates into aq-environment-templates (room for spec 0007 phase 1)`.

**Part C — provider quota.**
- Create `crates/aq-provider-quota/{Cargo.toml,src/lib.rs}`.
  - Its deps are `aq-dashboard-contract`, `base64`, `chrono`, `hex`, `libc`, `reqwest`, `serde`, `serde_json`, `sha2`, `tokio` and `zeroize`.
  - Its dev-deps are `tempfile` and `tokio` with `features = ["test-util", "macros", "rt"]`.
- `src/lib.rs` is the whole of `crates/aq-server/src/dashboard_api/quota.rs`, with these changes:
  - `use super::contract::{…}` becomes `use aq_dashboard_contract::{…}`, with the same names.
  - `use crate::provider_accounts::Account;` is removed. The `Account` struct (`provider_accounts.rs:8-16`, with its derive and doc lines) is pasted into the crate as `pub struct Account`.
  - `pub(crate) fn validate` and every `pub(super)` become `pub`.
- `crates/aq-server/src/dashboard_api/quota.rs` becomes exactly:

  ```rust
  //! Router-owned quota collection; moved to the aq-provider-quota crate.
  pub use aq_provider_quota::*;
  ```

- In `crates/aq-server/src/provider_accounts.rs`, delete the `Account` struct and add `pub use aq_provider_quota::Account;`.
- `config.rs:68` (`crate::dashboard_api::quota::CodexSource`) and `dashboard_api/runtime.rs` (`super::quota::Collector`) keep compiling through the re-export.
- Commit message: `refactor: move provider quota collection into aq-provider-quota (room for spec 0007 phase 1)`.

**Part D — playbook domain.**
- Create `crates/aq-playbook-domain/{Cargo.toml,src/lib.rs}`. Its deps are `aq-playbook-host` (as aq-server declares it), `axum`, `hex`, `serde`, `serde_json`, `sqlx`, `thiserror` and `uuid`.
- `src/lib.rs` is the whole of `crates/aq-server/src/playbooks/domain.rs`, plus the `impl IntoResponse for Error` block from `crates/aq-server/src/playbooks/api.rs:71-85`, with these imports: `use axum::{Json, http::StatusCode, response::{IntoResponse, Response}}; use serde_json::json;`.
- `crates/aq-server/src/playbooks/domain.rs` becomes:

  ```rust
  //! Fixed host contracts and typed grant scope; moved to the aq-playbook-domain crate.
  pub use aq_playbook_domain::*;
  ```

- Delete the impl block from `playbooks/api.rs`, and delete any import clippy then reports unused.
- Commit message: `refactor: move the playbook domain into aq-playbook-domain (room for spec 0007 phase 1)`.

**Steps:**
- [ ] **Step 1: Write the failing test** (Part A's test). Run `cd /home/jkern/dev/aq-platform/wt/swarm-p1-server && CARGO_TARGET_DIR=/home/jkern/dev/aq-platform/target-swarm-p1-server timeout 900 cargo test -p aq-row-retention --locked`. Expected: FAIL (the crate is missing).
- [ ] **Step 2: Do Parts A–D in order.** After each part, run:
  - `eval "$(scripts/test-db)"`
  - `timeout 900 cargo test -p <new crate> --locked`
  - the part's regression filter:
    - A: `timeout 900 cargo nextest run -p aq-server --test it_schema -E 'test(/^row_retention::/)'`
    - B: `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/placement|template/)'`
    - C: `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/provider_usage|quota|capacity/)'`
    - D: `timeout 900 cargo nextest run -p aq-server --test it_playbook -E 'test(/^playbook_/)'`

  Expected: PASS. Then commit the part.
- [ ] **Step 3: Check the budget.** Run `timeout 900 cargo test -p aq-server --lib --locked --no-run` (it compiles), then `python3 scripts/build-budget.py`. Expected: the aq-server lib is **≤ 108,000**, about 107,970. If it is above 108,000, a shim kept more than this task lists. Trim it before continuing.

---

### S1: Vendor aq-protocol 0.1.1 (aq-server) — Sonnet

**Files:**
- Create: `vendor/aq-protocol-0.1.1/`.
  - Copy it from aq-worker `crates/aq-protocol` at PR 1's merge commit, including `fixtures/`.
  - Add `vendor/aq-protocol-0.1.1/SOURCE.md` with one line: `Copied from ElectricJack/aq-worker <merge commit> crates/aq-protocol.`
- Delete: `vendor/aq-protocol-0.1.0/`.
- Modify: the root `Cargo.toml`:
  - `aq-protocol = { path = "vendor/aq-protocol-0.1.1", version = "=0.1.1" }`;
  - in `exclude`, replace `vendor/aq-protocol-0.1.0` with `vendor/aq-protocol-0.1.1`.

- [ ] **Step 1: Copy, edit and check.** Run these in order:
  1. `cargo update -p aq-protocol --offline`
  2. `timeout 900 cargo test -p aq-protocol --locked`. Expected: PASS, including `machine_fixture_round_trips` and `machine_key_rule_matches_worker_machine_v1`.
  3. `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::/)'`. Expected: PASS.
- [ ] **Step 2: Commit.** Commit message: `build: vendor aq-protocol 0.1.1 (machine wire types)`.

---

### S2: Migration 0101 and the `aq-machine-control` store (aq-server) — Sonnet

**Files:**
- Create: `crates/aq-server/migrations/0101_machine_control.sql`
- Create: `crates/aq-machine-control/{Cargo.toml,src/lib.rs}`. Its deps are `aq-protocol`, `chrono`, `sqlx`, `sqlx-postgres`, `thiserror` and `uuid` (as aq-server declares them).
- Modify: root `Cargo.toml` members; `crates/aq-server/Cargo.toml` (path dep `aq-machine-control = { path = "../aq-machine-control" }`).
- Test: `crates/aq-server/tests/it_access/machine_store.rs` (new module, listed in `tests/it_access/main.rs`).

**Migration, exactly:**

```sql
-- Spec 0007 phase 1: a service machine is the set of live nodes sharing a
-- machine key (the worker-machine/v1 key, claimed by slot 1); the owner's
-- desired state per machine; handoff requests; renewed context facts; and the
-- owner's sticky availability switch.
ALTER TABLE nodes ADD COLUMN machine_key TEXT CHECK (machine_key ~ '^[a-z0-9][a-z0-9-]{0,62}$');
ALTER TABLE nodes ADD COLUMN slot INTEGER CHECK (slot BETWEEN 1 AND 8);
ALTER TABLE nodes ADD CONSTRAINT nodes_machine_slot_pair CHECK ((machine_key IS NULL) = (slot IS NULL));
CREATE UNIQUE INDEX nodes_live_machine_slot ON nodes(machine_key, slot)
  WHERE machine_key IS NOT NULL AND revoked_at IS NULL;

CREATE TABLE machine_controls (
  machine_key TEXT PRIMARY KEY CHECK (machine_key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
  desired TEXT NOT NULL CHECK (desired IN ('running','draining','stopped')),
  generation BIGINT NOT NULL CHECK (generation >= 2),
  operation_id UUID NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  updated_by UUID NOT NULL REFERENCES users(id)
);

CREATE TABLE attempt_handoffs (
  attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
  node_id UUID NOT NULL REFERENCES nodes(id),
  reason TEXT NOT NULL CHECK (reason IN ('owner_stop','service_stop','restart')),
  requested_at TIMESTAMPTZ NOT NULL,
  candidate_operation_id UUID,
  candidate_oid TEXT CHECK (candidate_oid ~ '^[0-9a-f]{40}$'),
  note_comment_id UUID
);
CREATE INDEX attempt_handoffs_node ON attempt_handoffs(node_id);

CREATE TABLE context_qualification_renewals (
  renderer_digest TEXT PRIMARY KEY CHECK (renderer_digest ~ '^[0-9a-f]{64}$'),
  base_renderer_digest TEXT NOT NULL CHECK (base_renderer_digest ~ '^[0-9a-f]{64}$'),
  fact JSONB NOT NULL,
  observed_at_ms BIGINT NOT NULL,
  expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > observed_at_ms AND expires_at_ms - observed_at_ms <= 86400000),
  node_id UUID NOT NULL REFERENCES nodes(id),
  submission JSONB NOT NULL CHECK (octet_length(submission::text) <= 262144),
  created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX context_qualification_renewals_expiry ON context_qualification_renewals(expires_at_ms DESC);

CREATE TABLE owner_availability (
  singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
  available BOOLEAN NOT NULL,
  generation BIGINT NOT NULL CHECK (generation >= 2),
  operation_id UUID NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  updated_by UUID NOT NULL REFERENCES users(id)
);
UPDATE meta SET value='101' WHERE key='state_format_version';
```

**Defaults:**
- A machine key with no `machine_controls` row is `running` at generation 1. Each owner change writes generation `previous + 1`, so a stored row is always ≥ 2.
- With no `owner_availability` row, availability is **unset** at generation 1. Nothing is kept on and nothing is gated, as before this phase.
- A node with no `machine_key` is a legacy node. It is never gated and never controlled.

**Interfaces — Produces** (`crates/aq-machine-control/src/lib.rs`):

```rust
pub use aq_protocol::{DesiredState, HandoffReason, MAX_MACHINE_SLOTS, valid_machine_key};
pub const FORMAT: u32 = 101;   // first state_format_version with these tables
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")] NotFound,
    #[error("generation conflict")] Conflict { generation: u64 },
    #[error("machine key in use")] KeyInUse,
    #[error("invalid: {0}")] Invalid(&'static str),
    #[error(transparent)] Db(#[from] sqlx::Error),
}
#[derive(Debug, Clone, PartialEq, Eq)] pub struct MachineNode { pub node_id: Uuid, pub machine_key: Option<String>, pub slot: Option<u32> }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct Control { pub machine_key: String, pub desired: DesiredState, pub generation: u64, pub updated_at_ms: Option<u64>, pub operation_id: Option<Uuid> }
pub struct NewSlot<'a> { pub base_name: &'a str, pub worker_version: &'a str, pub protocol_version: u32 }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct MintedSlot { pub slot: u32, pub node_id: Uuid, pub created: bool }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct HandoffRow { pub attempt_id: Uuid, pub node_id: Uuid, pub reason: HandoffReason,
    pub candidate_operation_id: Option<Uuid>, pub candidate_oid: Option<String>, pub note_comment_id: Option<Uuid> }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct NodeService { pub node_id: Uuid, pub machine_key: String, pub slot: u32,
    pub desired: DesiredState, pub generation: u64, pub since_ms: u64 }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Availability { Unset, On, Off }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct OwnerAvailability { pub state: Availability, pub generation: u64, pub updated_at_ms: Option<u64>, pub operation_id: Option<Uuid> }

/// state_format_version >= FORMAT (the lineage_quarantined pattern).
pub async fn has_format(conn: &mut PgConnection) -> Result<bool, Error>;
pub fn slot_name(base: &str, slot: u32) -> String;                       // "<base>#<slot>", base truncated to 60 chars
pub async fn machine_of(conn: &mut PgConnection, node: Uuid) -> Result<MachineNode, Error>;       // NotFound when absent
/// Caller has no key: sets (key, slot 1), unless another live node holds (key, 1) → KeyInUse.
/// Caller already holds this key as slot 1: no change. Any other caller state → Invalid("not_slot_one").
pub async fn claim_key(tx: &mut PgConnection, node: Uuid, key: &str) -> Result<MachineNode, Error>;
/// Caller must hold a key as slot 1. Creates the missing live slots 2..=wanted; never re-issues an existing slot.
/// `hashes[i]` is the credential verifier for slot i+2; only the slots it creates use theirs.
pub async fn mint_slots(tx: &mut PgConnection, caller: &MachineNode, wanted: u32, new: &NewSlot<'_>,
    hashes: &[String], now: DateTime<Utc>) -> Result<Vec<MintedSlot>, Error>;
pub async fn machine_exists(conn: &mut PgConnection, key: &str) -> Result<bool, Error>;   // a live node holds the key
pub async fn control(conn: &mut PgConnection, key: &str) -> Result<Control, Error>;
/// Plain compare-and-swap: `expected_generation` must equal the current generation, else
/// Error::Conflict{generation: current}. On success writes desired, generation+1, operation_id (also when desired is unchanged).
pub async fn set_desired(tx: &mut PgConnection, key: &str, desired: DesiredState, expected_generation: u64,
    actor: Uuid, operation_id: Uuid, now: DateTime<Utc>) -> Result<Control, Error>;
/// Inserts an owner_stop row for every profiled attempt on the machine's live nodes that holds its task and whose
/// hot stop kind is 'unknown' or 'running'. ON CONFLICT DO NOTHING. Returns the attempt ids now pending.
pub async fn request_handoffs(tx: &mut PgConnection, key: &str, now: DateTime<Utc>) -> Result<Vec<Uuid>, Error>;
/// Attempts with any handoff row whose attempt still holds its task and whose stop kind is 'unknown' or 'running'.
pub async fn pending_handoffs(conn: &mut PgConnection, key: &str) -> Result<Vec<Uuid>, Error>;
/// Pending handoff counts per key (same predicate), for the swarm read.
pub async fn pending_counts(conn: &mut PgConnection, keys: &[String]) -> Result<BTreeMap<String, u32>, Error>;
/// Node-announced handoff; the attempt must belong to `node`, else Invalid("attempt_not_owned"). ON CONFLICT (attempt_id) DO NOTHING.
pub async fn record_handoff(conn: &mut PgConnection, node: Uuid, attempt: Uuid, reason: HandoffReason,
    now: DateTime<Utc>) -> Result<(), Error>;
pub async fn handoff_of(conn: &mut PgConnection, attempt: Uuid) -> Result<Option<HandoffRow>, Error>;
pub async fn set_handoff_candidate(conn: &mut PgConnection, attempt: Uuid, operation: Uuid, oid: &str,
    note_comment: Option<Uuid>) -> Result<(), Error>;        // only fills NULL columns; equal values are a no-op
/// desired == Running for the node's key; true for a legacy node (no key).
pub async fn accepts_new_work(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error>;
/// The service facts of the given nodes that hold a key (legacy nodes are absent).
pub async fn node_services(conn: &mut PgConnection, nodes: &[Uuid]) -> Result<Vec<NodeService>, Error>;
/// `machine` is a key or "node:<uuid>". Sets revoked_at on every live node of it and deletes its machine_controls row.
/// Returns the number of nodes revoked (0 when none was live).
pub async fn revoke_machine(tx: &mut PgConnection, machine: &str, now: DateTime<Utc>) -> Result<u32, Error>;
pub async fn owner_availability(conn: &mut PgConnection) -> Result<OwnerAvailability, Error>;
/// Compare-and-swap on the singleton, as set_desired.
pub async fn set_owner_availability(tx: &mut PgConnection, available: bool, expected_generation: u64,
    actor: Uuid, operation_id: Uuid, now: DateTime<Utc>) -> Result<OwnerAvailability, Error>;
```

**SQL details:**

- **"Running attempt"** is used by `request_handoffs`, `pending_handoffs` and `pending_counts`:

  ```sql
  SELECT a.attempt_id FROM execution_attempts a
    JOIN nodes n ON n.id = a.node_id
    JOIN profiled_authority_hot h USING (attempt_id)
  WHERE n.machine_key = $1 AND n.revoked_at IS NULL AND a.kind = 'profiled_v1' AND a.holds_task
    AND h.state->'stop'->>'kind' IN ('unknown','running')
  ORDER BY a.attempt_id LIMIT 64
  ```

- **`mint_slots`** inserts each node with explicit `machine_key = caller key`, `slot = n` and `name = slot_name(base, n)`.
  - It returns `MintedSlot { created: false }` for slots that already have a live node.
  - It refuses with `Error::Invalid("slots_out_of_range")` unless `1 ≤ wanted ≤ 8`.
  - It refuses with `Error::Invalid("not_slot_one")` unless `caller.slot == Some(1)`.
  - Copy the `INSERT INTO nodes` column list from `store.rs` `redeem_enrollment` (`store.rs:875`). Set `enrolled_at = now`.
- **`node_services`:**

  ```sql
  SELECT n.id, n.machine_key, n.slot, COALESCE(c.desired,'running') AS desired, COALESCE(c.generation,1) AS generation,
    (extract(epoch FROM COALESCE(c.updated_at, n.enrolled_at)) * 1000)::bigint AS since_ms
  FROM nodes n LEFT JOIN machine_controls c ON c.machine_key = n.machine_key
  WHERE n.id = ANY($1) AND n.machine_key IS NOT NULL
  ```

- **`revoke_machine`:**
  - For `node:<uuid>`: `UPDATE nodes SET revoked_at=$2 WHERE id=$1 AND revoked_at IS NULL`.
  - For a key: `UPDATE nodes SET revoked_at=$2 WHERE machine_key=$1 AND revoked_at IS NULL`, then `DELETE FROM machine_controls WHERE machine_key=$1`.
  - Any other string → `Invalid("machine")`.

Every query uses `sqlx::query`/`query_as` with binds, as `aq-task-comments` does.

- [ ] **Step 1: Write the failing tests** in `tests/it_access/machine_store.rs`.
  - Each test uses `aq_server::test_support::TestDb::create().await` and `db.pool().await`.
  - Seed a user and nodes with direct SQL, copying the seed shapes from `tests/it_access/worker_api.rs`.
  - Seed profiled attempts with hot state through `common::profiled::{fixture, insert_offered}`, as `tests/it_profiled/replacement_release.rs` (`granted`, `set_hot`) does.

  The tests:
  - `legacy_node_has_no_machine_and_accepts_work`: a node without a key → `machine_of` has `machine_key: None`; `accepts_new_work` is true; `node_services` omits it.
  - `claim_key_then_mint_slots_once`:
    - node "desk" claims `"desk"` → slot 1;
    - `mint_slots(wanted = 3)` returns slots 2 and 3, both `created: true`, named `"desk#2"` and `"desk#3"`;
    - a second call returns the same `node_id`s with `created: false`;
    - calling it from slot 2 gives `Invalid("not_slot_one")`, and so does `wanted = 9` (`Invalid("slots_out_of_range")`).
  - `claim_key_refuses_a_key_held_by_another_live_machine`: a second node claiming `"desk"` gives `KeyInUse`. After `revoke_machine("desk")` it succeeds.
  - `set_desired_is_compare_and_swap`: `set_desired(Stopped, 1)` → generation 2. `set_desired(Running, 1)` → `Conflict { generation: 2 }`.
  - `request_handoffs_selects_running_attempts_only`: seed two attempts with `holds_task` and hot `stop.kind` `running` and `stopped`. Only the running one is returned. A second call returns the same single id and inserts no duplicate.
  - `record_handoff_requires_the_owning_node`: another node's attempt gives `Invalid("attempt_not_owned")`.
  - `accepts_new_work_follows_desired`: `true` when running, `false` after `set_desired(Draining)`.
  - `revoke_machine_revokes_every_slot_and_clears_control` (Review Focus 8): after minting 3 slots and `set_desired(Stopped)`:
    - `revoke_machine("desk")` returns 3;
    - every node has `revoked_at`;
    - `control("desk")` is back to `{Running, 1}`;
    - `revoke_machine("node:<uuid of a legacy node>")` returns 1.
  - `owner_availability_defaults_unset_and_is_compare_and_swap`:
    - the default is `{Unset, 1}`;
    - `set_owner_availability(true, 1)` → `{On, 2}`;
    - `set_owner_availability(false, 1)` → `Conflict { generation: 2 }`.
  - `node_services_report_desired_and_since`: after `set_desired(Draining)`, every slot node reports `Draining` with generation 2 and `since_ms` = the update time.
- [ ] **Step 2: Run them.** Run `eval "$(scripts/test-db)"; timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^machine_store::/)'`. Expected: FAIL (missing crate or migration).
- [ ] **Step 3: Write the migration and implement the crate.**
- [ ] **Step 4: Run the tests and catalog checks.** Run the same command (PASS), plus:
  - `timeout 900 cargo nextest run -p aq-server --test it_schema -E 'test(/^(store|migration_catalog)::/)'`
  - `python3 scripts/migration_catalog.py`
  - `python3 scripts/test-migration-catalog.py`
  - `timeout 900 cargo test -p aq-server --lib store::tests::state_format_is_the_latest_migration`

  Expected: PASS.
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): migration 0101 and aq-machine-control store (spec 0007 phase 1)`.

---

### S3: `/v1/worker/slots` — claim the machine key and mint sibling slot nodes (aq-server) — Sonnet

**Files:**
- Create: `crates/aq-server/src/machine_api.rs`.
  - Declare the module in `lib.rs` next to `worker_api`.
  - Merge its routes into `worker_routes` in `worker_api.rs:22`, with `.merge(crate::machine_api::machine_routes(state.clone()))`.
- Modify: `protocol/commands/v1/routes.json`. Add `{"path": "/v1/worker/slots", "class": "node", "note": <same note string as /v1/worker/heartbeat>}`, keeping alphabetical order.
- Test: `crates/aq-server/tests/it_access/worker_api.rs` (existing module; reuse its `Harness`).

**Interfaces — Produces:**
- `pub fn machine_routes(state: AppState) -> Router<AppState>`, with body limit `16 * 1024`.
- `async fn slots(State(state): State<AppState>, headers: HeaderMap, ApiJson(req): ApiJson<SlotsRequest>) -> Result<Json<SlotsResponse>, AppError>`:
  1. `let auth = crate::worker_api::authenticate_context(&state, &headers).await?;`
  2. Validate the request:
     - `req.schema_version == 1`, else `AppError::BadRequest("schema_version")`;
     - `aq_protocol::valid_machine_key(&req.machine_key)`, else `AppError::BadRequest("machine_key must be 1-63 characters of [a-z0-9-], not starting with -")`;
     - `1 ..= MAX_MACHINE_SLOTS` contains `req.slots`, else `AppError::BadRequest("slots must be 1-8")`.
  3. Begin a transaction, then `crate::worker_api::lock_current_node(&mut tx, &auth)`. The lock means a revoked or racing caller can never mint.
  4. `let caller = claim_key(&mut tx, auth.id(), &req.machine_key)`.
  5. Generate `req.slots - 1` credentials with `crate::tokens::new_node_credential()` and their `verifier()` hashes.
  6. Call `mint_slots(&mut tx, &caller, req.slots, &NewSlot { base_name: &auth.node().name, worker_version: &auth.node().worker_version, protocol_version: auth.node().protocol_version }, &hashes, now)`, then commit.
  7. The response carries `credential: Some(..)` only for `created` slots, and `machine_key = req.machine_key`.
  - Error mapping:
    - `Error::KeyInUse` → `AppError::Conflict("machine key is in use by another machine; revoke it from the Swarm page or choose another key".into())`;
    - `Error::Invalid(m)` → `AppError::BadRequest(m.into())`;
    - `Conflict` → `AppError::Conflict("generation".into())`;
    - `NotFound` and `Db` → `AppError::Internal`.

- [ ] **Step 1: Write the failing tests** in `worker_api.rs`.
  - `slots_claim_key_and_mint_siblings_once_with_credentials`: enroll through the harness (the existing enroll helper), then POST `/v1/worker/slots` `{"schema_version":1,"machine_key":"desk","slots":3}` with the enrolled bearer.
    - Assert 200, `machine_key == "desk"`, and 2 slots, each with a credential starting `aqn_`.
    - Heartbeat succeeds with each sibling credential.
    - `SELECT machine_key, slot FROM nodes` shows `desk/1`, `desk/2` and `desk/3`.
    - A second call returns the same `node_id`s and no `credential` field.
  - `slots_refuses_sibling_caller_bounds_and_bad_key`:
    - a sibling bearer gets 400;
    - `slots: 0` and `slots: 9` get 400;
    - `machine_key: "Desk"` gets 400;
    - no bearer gets 401.
  - `slots_refuses_a_key_held_by_another_machine`: a second enrolled node claiming `"desk"` gets 409.
  - `slots_refused_after_machine_revoked` (Review Focus 8): after `aq_machine_control::revoke_machine("desk")`, the slot-1 bearer gets 401 (authentication refuses a revoked node).
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::slots_/)'`. Expected: FAIL (404).
- [ ] **Step 3: Implement and mount the route, and add the routes.json entry.** Keep `machine_api.rs` at 120 lines or fewer.
- [ ] **Step 4: Run them again.** Expected: PASS. Then run `timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`. Expected: PASS (the route list check).
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): /v1/worker/slots claims the machine key and mints its slot nodes`.

---

### S4: `/v1/worker/control`, `/v1/worker/handoffs` and the offer gate (aq-server) — Sonnet

**Files:**
- Modify: `crates/aq-server/src/machine_api.rs`
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs`. The change goes in `offers()`, immediately before the `redelivery::existing` call at the comment that reads `// Redelivery precedes new scheduling.` (line 354).
- Modify: `protocol/commands/v1/routes.json` (two `node` entries)
- Create: `crates/aq-server/src/profiled_execution/context_renewal.rs` (the stub), declared in `profiled_execution.rs`
- Test: `crates/aq-server/tests/it_access/worker_api.rs`; `crates/aq-server/tests/it_native/native_offers.rs` (existing module)

**Interfaces — Produces:**
- `async fn control(State, HeaderMap, ApiJson<ControlRequest>) -> Result<Json<ControlResponse>, AppError>`:
  1. Authenticate, then `machine_of`.
  2. A node without a key gets `AppError::Conflict("this node is not a service machine; enroll it with aq-worker service enroll".into())`.
  3. Otherwise call `control` and `pending_handoffs` on its key.
  4. `qualification` is `crate::profiled_execution::context_renewal::current_pin(&state, now_ms).await?`. Until S12 this is the stub, which returns `Ok(None)`.
  5. Record a heartbeat with `state.store.record_heartbeat(node.id, &node.worker_version, node.protocol_version, now)`. The control poll counts as liveness.
- `async fn handoffs(State, HeaderMap, ApiJson<HandoffNotice>) -> Result<Json<HandoffNotice>, AppError>`: authenticate, then `record_handoff(node.id, attempt, reason)`. It echoes the request. `Invalid` maps to 400.
- **Offer gate.** In `offers()`:

  ```rust
  let mut gate = state.store.pool.acquire().await.map_err(|_| refused())?;
  if aq_machine_control::has_format(&mut gate).await.map_err(|_| refused())?
      && !aq_machine_control::accepts_new_work(&mut gate, auth.id()).await.map_err(|_| refused())?
  {
      tracing::info!(node_id = %auth.id(), "machine is not running (draining or stopped); no new offers");
      return Ok(page);
  }
  drop(gate);
  ```

  Cancellations are already in `page`, so a stopped machine still receives them.
- **Stub:** `crates/aq-server/src/profiled_execution/context_renewal.rs` with `pub async fn current_pin(_: &AppState, _: u64) -> Result<Option<aq_protocol::QualificationPin>, AppError> { Ok(None) }`.

- [ ] **Step 1: Write the failing tests.**
  - In `worker_api.rs`:
    - `control_reports_desired_state_and_pending_handoffs`:
      - a node that claimed `"desk"` gets `desired = "running"`, `generation = 1` and an empty `handoff`;
      - after a direct `set_desired(Stopped)` and a seeded running attempt plus `request_handoffs`, the response carries that attempt id, `"stopped"` and generation 2;
      - `last_seen_at` advances.
    - `control_refuses_a_legacy_node`: an enrolled node with no key gets 409.
    - `machine_control_generation_conflicts_and_idempotent_handoffs` (Review Focus 3):
      - two `request_handoffs` calls leave one row;
      - `set_desired` with a stale generation conflicts;
      - POST `/v1/worker/handoffs` twice gives 200 twice and one row;
      - another node's attempt gives 400.
  - In `it_native/native_offers.rs`, `stopped_machine_gets_no_new_offer_but_keeps_cancellations`:
    - copy the setup of the nearest existing offer test in that module, then `claim_key` the polling node;
    - after `set_desired(Stopped)`, the poll returns zero offers;
    - after `set_desired(Running)` at the new generation, the offer appears.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::(control|machine_control)/)'` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::stopped_machine/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** Add the routes.json entries for `/v1/worker/control` and `/v1/worker/handoffs`, and add the stub.
- [ ] **Step 4: Run them again.** Expected: PASS. Also run:
  - `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'` (no regressions);
  - the `commands::` check.
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): machine control route, handoff notices and the offer gate`.

---

### S5: Fill the phase 2 seam — service states, machine control and server-key grouping in the swarm read (aq-server) — Sonnet

This replaces revision 1's `/api/workers` fields. The machine control facts reach the dashboard through `GET /api/v1/swarm`, which the phase 2 Swarm pane already polls every 10 s.

**Files:**
- Modify: `crates/aq-swarm/Cargo.toml`. Add `aq-machine-control = { path = "../aq-machine-control" }`.
- Modify: `crates/aq-swarm/src/service.rs`. Replace the body of `states`, and add `controls`.
- Modify: `crates/aq-swarm/src/assemble.rs`. Add `NodeExtra.machine_key`, `SwarmInput.controls` and the grouping rule.
- Modify: `crates/aq-dashboard-contract/src/swarm_contract.rs`. Add `MachineDesired`, `MachineControl` and `SwarmMachine.control`.
- Modify: `crates/aq-server/src/dashboard_api/swarm.rs`. Load and pass `controls`.
- Regenerate: `protocol/dashboard-read/v1/schema.json` with `cargo run -p aq-server --example dashboard_contract --offline`.
- Modify: `protocol/dashboard-read/v1/fixtures/swarm.json`. Every machine gets `"control"`. The keyed machine `"desktop"` gets `{"desired":"running","generation":"1","since":null,"pending_handoffs":0}`; every other machine gets `null`.
- Modify: `protocol/dashboard-read/v1/README.md`. Add the paragraph below after the `swarm.read@1` paragraph.
- Test: `crates/aq-swarm/src/tests.rs` (pure unit tests); `crates/aq-server/tests/it_dashboard/dashboard_swarm.rs` (existing module)

**Interfaces — Produces:**

```rust
// aq-dashboard-contract (swarm_contract.rs)
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineDesired { Running, Draining, Stopped }
/// The owner's control of a service machine (spec 0007 phase 1).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineControl {
    pub desired: MachineDesired,
    /// Decimal compare-and-swap generation for worker.start/stop/drain.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub generation: String,
    pub since: Option<Timestamp>,
    pub pending_handoffs: u32,
}
// SwarmMachine gains, after `cost`:
    /// Present only for a service machine (a server-held key); None for legacy and `node:` machines.
    pub control: Option<MachineControl>,

// aq-swarm (service.rs)
pub struct ServiceFact { pub state: ServiceState, pub since_ms: u64 }        // unchanged
pub struct ControlFact { pub desired: MachineDesired, pub generation: u64, pub since_ms: u64, pub pending_handoffs: u32 }
/// Phase 1 body: every node that holds a key reports its machine's desired state
/// (running → Running, draining → Draining, stopped → Stopped); legacy nodes are absent (unreported).
pub async fn states(conn: &mut sqlx::PgConnection, nodes: &[Uuid]) -> sqlx::Result<BTreeMap<Uuid, ServiceFact>>;
/// One entry per machine key held by these nodes.
pub async fn controls(conn: &mut sqlx::PgConnection, nodes: &[Uuid]) -> sqlx::Result<BTreeMap<String, ControlFact>>;

// aq-swarm (assemble.rs)
pub struct NodeExtra { /* existing fields */ pub machine_key: Option<String> }   // loaded from nodes.machine_key
pub struct SwarmInput { /* existing fields */ pub controls: BTreeMap<String, ControlFact> }
```

**Rules:**
- **Store format.** `states` and `controls` return empty maps when `aq_machine_control::has_format` is false. Map `aq_machine_control::Error::Db(e)` to `e`, and any other error to `sqlx::Error::Protocol("machine control".into())`.
- **`NodeExtra::load`.** Add `n.machine_key` to its SQL only when the format is present. Otherwise select `NULL::text AS machine_key`.
- **Grouping key in `assemble`.** Replace the `let key = …` expression:

  ```rust
  let held: BTreeSet<&str> = extras.iter().filter_map(|e| e.machine_key.as_deref()).collect();
  // per node:
  let key = extra.and_then(|e| e.machine_key.clone())
      .or_else(|| facts.and_then(|f| f.report.machine.key.clone())
          .filter(|k| !k.is_empty() && !held.contains(k.as_str())))
      .unwrap_or_else(|| format!("node:{id}"));
  ```

  Compute `held` before the node loop.
- **`SwarmMachine.control`** is `controls.get(&key).map(|c| MachineControl { desired, generation: c.generation.to_string(), since: Some(timestamp(c.since_ms)?), pending_handoffs })`.
- The existing hold and state rules (`worker_stopped`, `worker_draining`, machine `stopped`) now fire from `states`. Do not change them.
- **README paragraph:** "Phase 1 service machines: a node enrolled with `aq-worker service` holds a server-side machine key (claimed by its slot 1). That key groups the machine's nodes ahead of any reported key, and a reported key that a service machine holds no longer groups other nodes. A service machine carries `control` (desired state, compare-and-swap generation, its time, pending handoffs). `control` is `null` for legacy and `node:` machines, which the worker actions refuse. Each service node's `service.state` is its machine's desired state."

- [ ] **Step 1: Write the failing tests.**
  - In `crates/aq-swarm/src/tests.rs`, building `SwarmInput` the way the existing tests do:
    - `service_machine_groups_by_server_key_and_carries_control`: three nodes with `machine_key = Some("desk")`, of which only one has a report (key `"desk"`), form one machine `"desk"` with `control.generation == "1"`.
    - `reported_key_held_by_a_service_machine_is_not_merged`: a legacy node reporting key `"desk"` while a service machine holds `"desk"` becomes `node:<uuid>`, with `control: None`.
    - `stopped_service_nodes_hold_worker_stopped_and_machine_is_stopped`.
    - `draining_service_node_holds_worker_draining`.
    - `legacy_machine_has_no_control`.
  - In `dashboard_swarm.rs`, `service_machine_reports_control_and_service_state`:
    - claim `"desk"` and mint 2 slots;
    - `set_desired(Draining)`;
    - GET `/api/v1/swarm` shows one machine `"desk"` with `control.desired == "draining"` and `generation == "2"`;
    - each node has `service.state == "draining"` and a `worker_draining` hold.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-swarm --locked` and `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_swarm::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement**, regenerate the schema, and update the fixture and README.
- [ ] **Step 4: Run the tests again.** Expected: PASS. Then run:
  - `cargo run -p aq-server --example dashboard_contract --offline -- --check` (no drift);
  - `python3 protocol/dashboard-read/v1/check.py` (PASS);
  - `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_(swarm|capacity)/)'` (no regressions).
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): service states, machine control and server-key grouping in the swarm read`.

---

### S6: Worker actions `worker.start` / `worker.stop` / `worker.drain` / `worker.revoke`, and capabilities (aq-server) — Sonnet

**Files:**
- Create: `crates/aq-server/src/machine_actions.rs`, declared in `lib.rs`.
- Modify: `crates/aq-server/src/dashboard_api/actions/existing.rs`. Add `crate::machine_actions::register(&mut registry);` in `registry()` and `crate::machine_actions::schemas(&mut registry);` in `schemas()`.
- Modify: `crates/aq-server/src/dashboard_api/service.rs`. After `capability("swarm.read", a.owner, "The swarm requires an owner account")`, add:
  - `capability("swarm.worker.control", a.owner, "Worker control requires an owner account")`;
  - `capability("swarm.worker.enroll", a.owner, "Adding a machine requires an owner account")`.
- Regenerate:
  - `protocol/dashboard-actions/v1/schema.json` with `cargo run -p aq-server --example dashboard_actions_contract`;
  - `protocol/commands/v1/commands.json` with `AQ_REGENERATE_COMMANDS=1 timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`.
- Create fixtures: `protocol/dashboard-actions/v1/fixtures/request-worker-stop.json`, `request-worker-start.json`, `request-worker-drain.json` and `request-worker-revoke.json`.
- Modify: `protocol/dashboard-actions/v1/README.md`. Add a "Worker control" section with the four kinds, their payloads, subject and dispositions, and the capability names.
- Test: `crates/aq-server/tests/it_dashboard/dashboard_worker_actions.rs` (new module in `it_dashboard/main.rs`).

**Interfaces — Produces:**

```rust
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerControl {
    pub operation_id: Uuid,
    /// SwarmMachine.id of a service machine (a machine key).
    #[schemars(regex(pattern = "^[a-z0-9][a-z0-9-]{0,62}$"))]
    pub machine: String,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_generation: String,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerRevoke {
    pub operation_id: Uuid,
    /// SwarmMachine.id: a machine key, or "node:<uuid>" for a single legacy node.
    #[schemars(regex(pattern = "^([a-z0-9][a-z0-9-]{0,62}|node:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$"))]
    pub machine: String,
}
pub(crate) trait Desired: Send + Sync + 'static {
    const KIND: &'static str; const DESIRED: aq_machine_control::DesiredState; const HANDOFF: bool;
}
pub(crate) struct Start; pub(crate) struct Stop; pub(crate) struct Drain;   // impl Desired: "worker.start"/Running/false, "worker.stop"/Stopped/true, "worker.drain"/Draining/false
pub(crate) struct SetDesired<D>(std::marker::PhantomData<D>);              // one generic ActionKind impl for the three
pub(crate) struct RevokeMachine;
pub(crate) fn register(r: &mut Registry);   // the four kinds, .expect("worker control adapter")
pub(crate) fn schemas(r: &mut Registry);
```

`impl<D: Desired> ActionKind for SetDesired<D>` has:
- `type Payload = WorkerControl`;
- `const KIND = D::KIND`;
- `const GRANT = "worker.control"`, which is not in the agent grant vocabulary, so the kind is owner-only;
- `const TARGET = aq_command_registry::TargetRule::Subject`.

`RevokeMachine` has `Payload = WorkerRevoke`, `KIND = "worker.revoke"`, `GRANT = "worker.control"` and the same `TARGET`. Model the file on `crate::agent_access::questions::action` (`agent_access/questions/action.rs`).

- **`authorize`** (both kinds):
  1. `existing::owner(state, user).await?`.
  2. These must hold, else `ApiError::invalid()`:
     - `request.operation_id == p.operation_id`;
     - `request.subject.entity_type == "machine"` and `request.subject.id == p.machine`;
     - `request.subject.revision.is_none()`;
     - `request.expected_versions.is_empty()`;
     - for `SetDesired`, `expected_generation` parses as a `u64` in `1..=i64::MAX` whose decimal string equals the input (copy `revision()` from `questions/action.rs`).
  3. The machine must exist, else `ApiError::for_status(StatusCode::NOT_FOUND)`:
     - for `SetDesired`, `aq_machine_control::machine_exists(&mut conn, &p.machine)`;
     - for `RevokeMachine`, a key exists by the same call, or `node:<uuid>` names a node with `revoked_at IS NULL`.
  4. Return `Ok(Scope::AllAuthorized)`.
- **`authorize_lookup`:** `existing::owner(state, user).await`. Refuse when `scope` is not `AllAuthorized`, or when an operation's `subject_refs` is not exactly one `machine`.
- **`reconcile`** (read-only):
  - For `SetDesired`, the `machine_controls` row of `operation.subject_refs[0].id` whose `operation_id = operation.operation_id`. If it exists → the confirmed outcome below; otherwise `Ok(None)`.
  - For `RevokeMachine`, `Ok(None)`.
- **`execute`** for `SetDesired`, in one transaction:
  1. `set_desired(key, D::DESIRED, expected, user.id, p.operation_id, now)`.
  2. If `D::HANDOFF`, then `request_handoffs(key, now)`.
  3. Commit.
  - `Ok(c)` → `Outcome::confirmed(vec![existing::entity("machine_control", p.operation_id, Some(c.generation.to_string()))], vec![existing::entity("machine", &p.machine, Some(c.generation.to_string()))])`.
  - `Err(Conflict { generation })` → `Outcome::conflict(BTreeMap::from([("machine".into(), generation.to_string())]))`.
  - Any other error → `existing::failure(AppError::Internal)`.
- **`execute`** for `RevokeMachine`: one transaction, `revoke_machine(&p.machine, now)`, then commit.
  - `Ok(_)` → `Outcome::confirmed(vec![existing::entity("machine_revocation", p.operation_id, None)], vec![existing::entity("machine", &p.machine, None)])`. Use no revision: the dashboard accepts only revisions matching `^[1-9][0-9]{0,39}$`.
  - When no node was live (`Ok(0)`), the outcome is still confirmed: revoking twice is idempotent, as `/api/workers/{id}/revoke` is.

Request fixture `request-worker-stop.json`:

```json
{"operation_id":"5b0c2a9e-1f1e-4c55-9d7e-000000000001","kind":"worker.stop",
 "subject":{"type":"machine","id":"desktop"},"expected_versions":{},
 "reason":"Stop the desktop for gaming",
 "payload":{"operation_id":"5b0c2a9e-1f1e-4c55-9d7e-000000000001","machine":"desktop","expected_generation":"1"}}
```

- `request-worker-start.json` and `request-worker-drain.json` use the same shape with their own kind and operation ids `…0002` and `…0003`, and no `reason`.
- `request-worker-revoke.json` uses operation id `…0004`, kind `worker.revoke` and `"payload":{"operation_id":"…0004","machine":"desktop"}`.
- `reason` is optional for all four kinds.

- [ ] **Step 1: Write the failing tests** in `dashboard_worker_actions.rs`, using the harness pattern of `it_dashboard/dashboard_actions.rs`:
  - `owner_stop_sets_stopped_and_requests_handoffs`: seed a machine `"desktop"` (claim the key) with a running attempt and POST the fixture body. Expect:
    - 200 `confirmed`;
    - `machine_controls.desired = 'stopped'` at generation 2;
    - one `attempt_handoffs` row with reason `owner_stop`.
  - `drain_and_start_change_state_without_handoffs`.
  - `stale_generation_is_a_conflict` (Review Focus 3): stop at "1", then start at "1" → `conflict`, with `current_versions.machine == "2"`.
  - `legacy_machine_is_not_found`: `worker.stop` on `node:<uuid>` or an unknown key → 404.
  - `revoke_revokes_every_slot_node`: claim `"desktop"` and mint 3 slots. `worker.revoke` → confirmed, and all three nodes are revoked. The swarm read no longer lists the machine. Repeating it → confirmed again.
  - `non_owner_is_refused`.
  - `bootstrap_lists_worker_capabilities`: bootstrap includes `swarm.worker.control` and `swarm.worker.enroll`, both at version `"1"` and enabled for the owner.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_worker_actions::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement**, regenerate both protocol files, and write the fixtures and the README section. Then run:
  - `python3 protocol/dashboard-actions/v1/check.py` (PASS);
  - `cargo run -p aq-server --example dashboard_actions_contract -- --check` (no drift).
- [ ] **Step 4: Run the tests again.** Expected: PASS. Then run:
  - `timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'` (PASS, including `every_action_kind_is_a_command_with_a_declared_grant`);
  - `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_actions::/)'` (no regressions).

  Keep `machine_actions.rs` at 200 lines or fewer.
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): worker start/stop/drain/revoke owner actions and the swarm.worker capabilities`.

---

### S7: Successor base from the handoff commit, and the handoff note (aq-server) — stronger model

A replacement for an attempt with an `attempt_handoffs` row must start from the predecessor's saved work, not from its original base. The handoff note goes on the task thread.

**Files:**
- Create: `crates/aq-server/src/profiled_execution/handoff.rs`, declared in `profiled_execution.rs`.
- Modify: `crates/aq-server/src/profiled_execution/source.rs`. Add a variant to the private selection of `observe_source` (see below).
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs`. Change:
  - the `let (token, fact) = if let Some(replacement) = &replacement { … }` block (≈ lines 494-530);
  - the second observation (≈ lines 562-570).
- Test: `crates/aq-server/tests/it_profiled/replacement_handoff.rs` (a new module in `it_profiled/main.rs`; base it on `replacement_source.rs`'s `setup()` and `common::source_exports::setup()`).

**Interfaces — Produces:**

```rust
// handoff.rs
pub struct HandoffBase { pub oid: String, pub retention_handle: Uuid, pub predecessor: Uuid }
/// Some when `replacement.predecessor` has an attempt_handoffs row and a recorded result report
/// whose first confirmed source receipt is found. Creates (idempotently) an aq-git candidate:
///   operation id  = Uuid::new_v5(&predecessor, b"aq.swarm.handoff.candidate")
///   base          = the predecessor's admitted source commit (r.spec.source.resolved_object)
///   source_receipts = [that confirmed receipt]
///   commit message = "AQ: handoff of task {task} from attempt {predecessor}\n\n{note}"
/// stores candidate_operation_id/oid in attempt_handoffs, and posts the note once as a task comment:
///   aq_task_comments::add(pool, Uuid::new_v5(&predecessor, b"aq.swarm.handoff.note"),
///       Thread { project_id, task_id }, &Author::System { component: "aq.swarm.handoff".into() },
///       &format!("Handoff from attempt {predecessor} ({reason}). {conclusion}"), now)
/// None when no handoff row exists, when the store format is below aq_machine_control::FORMAT,
/// when `state.delivery()` is None (log warn once), or when the candidate is not Ready (log warn with
/// the conflict step); the caller then keeps the old behaviour.
pub async fn successor_base(state: &AppState, task: Uuid, replacement: &Replacement) -> Result<Option<HandoffBase>, AppError>;

// source.rs
pub async fn observe_handoff(state: &AppState, project: &FrozenProjectBinding, policy: &ProjectPolicyRecord,
    base: &HandoffBase) -> Result<Observation, AppError>;
```

**Algorithm for the workflow change:**

```rust
let handoff = match &replacement {
    Some(r) => on_heap(|| handoff::successor_base(state, task.task_id, r)).await?,
    None => None,
};
let (token, fact) = if let Some(base) = &handoff {
    // A fresh export from the saved commit: a new operation, never the predecessor's export.
    let observed = on_heap(|| source::observe_handoff(state, &compiled.frozen, &policy_record, base)).await?;
    let operation = source_operation(policy.project_id, &task, record.revision, generation,
        config.project_policy_ref(policy.project_id).map_err(|_| refused())?, Some(base.oid.as_str()))?;
    /* exactly the existing `match reservation(...)` Pending/Verified arms */
} else if let Some(replacement) = &replacement {
    /* existing retained_source branch, unchanged */
} else {
    /* existing first-attempt branch, unchanged */
};
```

The second observation keeps `source::observe_retained(…, token.request())` for both replacement kinds. On the handoff path, `token.request().base` is the handoff commit, and `operation_id` is the new export, which is its retained byte owner.

**`observe_handoff`:**
- It calls `observe_source` with a new internal parameter `Fixed { object: base.oid, retention_handle: base.retention_handle }`.
- `selection` becomes `SourceSelection::Immutable { object: ObjectId { format: Sha1, hex: base.oid } }`, and `retention_handle = base.retention_handle`.
- It adds `evidence.insert("handoff", json!({"predecessor": base.predecessor, "candidate_operation_id": base.retention_handle}))`.
- Model it on the existing D1 `publication_head` branch (`source.rs` ≈ 150-165), which already hydrates a candidate commit from its central retention handle.

**Guardrails (must hold):**
- The 0053/0095 shared-export guard is untouched. The handoff path never reuses the predecessor's export operation.
- This task does not change `assess()` or the budgets. S8 exempts `owner_stop`/`service_stop` handoffs from them; this task only changes the successor's source.
- `create_candidate` is idempotent by operation id, so a retried poll replays the same candidate. Check `delivery.frozen_inputs(op)` first, as `publication.rs:344-350` does.
- The task comment is posted at most once: its operation id is deterministic, and `note_comment_id` is set.

- [ ] **Step 1: Write the failing tests** in `replacement_handoff.rs`.
  - `handoff_successor_starts_from_saved_commit`. Use the synthetic Git owner of `replacement_source.rs`:
    - a predecessor with a `Failed` report whose retained source adds `saved.txt`;
    - an `attempt_handoffs(owner_stop)` row;
    - a poll then offers a successor whose `SourceAdmission.resolved_object.hex` equals the candidate oid, not the predecessor's base;
    - `attempt_handoffs.candidate_oid` is set;
    - one task comment with `author.kind = "system"` contains `"Handoff from attempt"`.
  - `plain_failure_successor_keeps_predecessor_export`: the same, but with no handoff row. The successor shares the predecessor's export operation. This is the regression guard for `successor_reuses_predecessor_export_only_with_exact_release_receipt`.
  - `handoff_poll_retry_replays_candidate_and_single_comment`: two polls produce one candidate operation and one comment.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^replacement_handoff::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** Keep `handoff.rs` at 140 lines or fewer.
- [ ] **Step 4: Run them again.** Expected: PASS. Then run `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^(replacement_|profiled_results::)/)'` (no regressions).
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): a handed-off attempt's successor starts from its saved commit, with the handoff note on the task`.

---

### S8: Owner and service handoffs are exempt from the lineage budgets (owner answer Q1) (aq-server) — stronger model

An `owner_stop` or `service_stop` handoff must not count against:
- the task's retry budget;
- the lineage time limit;
- the sandbox minutes.

This applies both to B4 and to the selection policy. A `restart` handoff (crash or reboot salvage) is still charged, so a crash loop stays bounded.

**Why the count cannot simply be lowered:** the real started count, `budget.attempts_started`, is compared for exact equality in four places:
- `intelligence/resolver.rs:152` and `:568`;
- `phase_execution/owner_core.rs:194-210`;
- `producer/build.rs:225`.

It is also stored under `CHECK (attempts_started > 0)` in `profiled_replacement_releases` (migration 0052). So the exemption is carried as a separate `exempt_attempts` count.

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/replacement.rs`
- Modify: `crates/aq-selection/src/domain.rs` (`BudgetSnapshot`, ≈ L182-196), `crates/aq-selection/src/select.rs` (`global_budget`, ≈ L13) and `crates/aq-selection/Cargo.toml` (version `0.1.5`)
- Modify: the version pin `=0.1.4` → `=0.1.5` in `crates/aq-server/Cargo.toml`, `crates/aq-swarm/Cargo.toml` and `crates/aq-intelligence-config/Cargo.toml`. Then run `cargo update -p aq-selection --offline`.
- Modify: `crates/aq-server/src/intelligence/resolver.rs` (`ReplacementBudget` at L29-33; `BudgetSnapshot` construction at ≈ L249-277)
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs` (`selection()`, ≈ L198-206)
- Modify: `crates/aq-server/src/agent_access/wakes.rs` (the started count, ≈ L413-416)
- Test: `replacement.rs` unit tests; `crates/aq-selection/tests/selection.rs`; `crates/aq-server/tests/it_profiled/replacement_handoff.rs`

**Changes, exactly:**
1. **`Prior`** gains `pub exempt: bool`, documented as "An owner_stop or service_stop handoff (spec 0007 Q1): not charged to any lineage budget."
2. **`PRIORS`** (≈ L201-211) gains the select column `EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop')) AS exempt`.
   - Keep a second constant, `PRIORS_PRE_101`, identical except that the column is `FALSE AS exempt`.
   - `load` (≈ L239-257) runs `PRIORS` when `state_format_version >= 101`, else `PRIORS_PRE_101`. Read the version with the `lineage_quarantined` query.
   - `decode` reads `exempt`.
3. **`Replacement`** gains `pub exempt_attempts: u32`.
4. **`assess`** (≈ L166-188). With `ordered` sorted as today:
   - `attempts_started = ordered.len()`, unchanged;
   - `exempt_attempts = ordered.iter().filter(|p| p.exempt).count()`;
   - the retry check uses `attempts_started - exempt_attempts >= max_attempts`;
   - **elapsed** is the sum over the non-exempt attempts of `next.offered_at_ms - this.offered_at_ms`, using `now` for the last attempt. With no exempt attempts this equals today's `now - first`;
   - **sandbox minutes** charge only non-exempt priors (`&& !p.exempt` in the existing filter).
5. **aq-selection.**
   - `BudgetSnapshot` gains `#[serde(default, skip_serializing_if = "is_zero")] pub exempt_attempts: u32`, with `fn is_zero(v: &u32) -> bool { *v == 0 }`. A zero value keeps every existing decision JSON, digest and fixture byte-identical.
   - In `global_budget`, compare `input.budget.attempts_started.saturating_sub(input.budget.exempt_attempts) >= policy.max_attempts`.
6. **Resolver.**
   - `ReplacementBudget` gains `exempt_attempts: u32`.
   - The `BudgetSnapshot` construction sets `exempt_attempts: replacement.map_or(0, |r| r.exempt_attempts)`.
   - `workflow.rs` `selection()` passes `exempt_attempts: r.exempt_attempts`.
   - Elapsed needs no selection change: the B4 elapsed already flows into `BudgetSnapshot.elapsed_ms` and every consumer downstream (`pinned.rs:376-383`, `facts.rs:217-224`, `owner_core.rs:235-243`, `continuation_policy.rs:16-26`).
7. **`wakes.rs`.** When the format is ≥ 101, the started count adds `AND NOT EXISTS (SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=e.attempt_id AND x.reason IN ('owner_stop','service_stop'))`, so the remaining retries an agent is told match B4. Leave its elapsed estimate as it is.

- [ ] **Step 1: Write the failing tests.**
  - In `replacement.rs` `mod tests`, using `failed_prior(offered_at_ms)`:
    - `owner_and_service_handoffs_are_exempt_restart_counts` (Review Focus 6):
      - three failed priors at 0, 10 min and 20 min, with the first two `exempt` → `Replace` with `attempts_started == 3` and `exempt_attempts == 2`;
      - the same priors with none exempt → `Blocked("lineage_retry_budget_exhausted")`.
    - `exempt_windows_are_not_lineage_time`: priors at 0 and 23 h, the first exempt, `now = 25 h` → `Replace` with `elapsed_ms == 2 h`. With neither exempt → `Blocked("lineage_time_budget_exhausted")`.
    - `exempt_attempts_cost_no_sandbox_minutes`.
    - `no_exempt_priors_match_previous_values`: `elapsed_ms == now - first` and `sandbox_ms` as before, on the existing `failed_attempt_is_replaced_within_retry_budget` inputs.
  - In `crates/aq-selection/tests/selection.rs`, modelled on `lineage_retry_cost_and_replacement_budgets_cannot_reset_on_fallback` (≈ L156):
    - `exempt_attempts_do_not_count_against_policy_max_attempts`: with `attempts_started 3`, `exempt_attempts 2` and policy `max_attempts 3` → resolved. With `exempt_attempts 0` → refused with `lineage_retry_budget_exhausted`.
    - `budget_snapshot_without_exempt_serializes_unchanged`: the JSON of a snapshot with `exempt_attempts: 0` has no `exempt_attempts` key.
  - In `replacement_handoff.rs`, `owner_stop_handoffs_do_not_exhaust_the_retry_budget`: three failed attempts, each with an `owner_stop` row → `assess_task` returns `Replace`. Change one row's reason to `restart` → still `Replace`, with `exempt_attempts == 2`.
- [ ] **Step 2: Run them.** Run:
  - `timeout 900 cargo test -p aq-server --lib --locked profiled_execution::replacement`
  - `timeout 900 cargo test -p aq-selection --locked`
  - `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^replacement_handoff::owner_stop/)'`

  Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS. Then run, with no regressions:
  - `timeout 900 cargo test -p aq-selection --locked --test task_baseline`, which compares against the frozen 0.1.3 fixture;
  - `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^(replacement_|profiled_results::)/)'`;
  - `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^(never_granted|granted_closure|native_initial_grant)::/)'`;
  - `timeout 900 cargo test -p aq-server --lib --locked replacement::tests::replacement_sql_matches_migrated_schema`.
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): owner and service handoffs are not charged to lineage budgets (spec 0007 Q1)`.

---

### S9: A refused successor never fails the poll or burns decision rows (owner answer Q1) (aq-server) — stronger model

**The wedge today:**
- B4 passes a successor that the predecessor's pinned selection policy will refuse. aq-ms1's policy has `max_attempts 2` and `max_lineage_elapsed_ms 1 h`, against B4's 3 and 24 h.
- `offers()` then turns the refusal into `Err(refused())` at `workflow.rs:473-477`, a 409 for the whole poll. MS1.md, "Known limits and owners", records this.
- The same task wins every poll, and every refused selection inserts an `intelligence_decisions` row (`resolver.rs:297-330`).
- That table has a global cap of 1,024 rows and is never pruned. An always-on service would fill it and stop all selection.

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/continuation_policy.rs`. Add `pinned_limits`.
- Modify: `crates/aq-server/src/profiled_execution/replacement.rs` (`assess_task_for_revision`, ≈ L350-366)
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs` (≈ L472-477) and `producer/build.rs` (≈ L219-224)
- Modify: `crates/aq-server/src/intelligence/resolver.rs` (`ResolveRequest`; the insert block ≈ L297-330)
- Test: `crates/aq-server/tests/it_profiled/replacement_policy_limits.rs` (new module in `it_profiled/main.rs`; add it to the nextest `selection-fixture` filter if it builds a selection fixture); `crates/aq-server/tests/it_native/native_offers.rs`

**Changes:**
1. **Pre-check, the real skip.**
   - Add `pub(crate) async fn pinned_limits(pool: &PgPool, attempt: Uuid) -> Result<Option<(u32, u64)>, AppError>` to `continuation_policy.rs`.
     - It returns the predecessor's pinned `SelectionPolicy` `(max_attempts, max_lineage_elapsed_ms)`.
     - Load it as `load_locked` does: `profiled_launches.launch_manifest->'intelligence_profile'`, `profiled_launch_artifacts`, `descriptor.verify`, then `decode`. Use no binding check and no `FOR SHARE`.
     - Any missing row or decode failure gives `Ok(None)`.
   - In `assess_task_for_revision`, after the revision check, with `charged = r.attempts_started - r.exempt_attempts`:
     - if `charged >= max_attempts` → `Assessment::Blocked("replacement_policy_retry_budget_exhausted")`;
     - else if `r.elapsed_ms >= max_lineage_elapsed_ms` → `Assessment::Blocked("replacement_policy_time_budget_exhausted")`.

     This mirrors `aq-selection` `global_budget`. `first_eligible_task` (`workflow.rs:252-285`) already skips a `Blocked` task and tries the next one.
2. **Backstop.**
   - At `workflow.rs:473-477`, when `decision.outcome.resolved` is `None` and `replacement.is_some()`: log at `warn` with `task_id`, `predecessor` and the outcome's reason codes (the field `global_budget` fills), with the message `"replacement refused by selection; no offer this poll"`, and `return Ok(page)`.
   - Do the same for the second selection's refusal in `build::assemble` (`build.rs:219-224`). There, return a value the caller turns into `Ok(page)`.
   - A first attempt keeps today's behaviour.
3. **No decision row for a refused poll selection.**
   - `ResolveRequest` gains `pub record_refused: bool`. Every existing construction sets it to `true`; only `producer::workflow::selection` sets it to `false`.
   - In `resolve_with_principal`, when `!request.record_refused && decision.outcome.resolved.is_none()`, return `Ok(decision)` before the retention-limit check and `subject.insert`, and roll the transaction back.
   - If an existing test asserts a decision row for a refused poll, keep `record_refused: true` for that path, and report it in the PR body.

- [ ] **Step 1: Write the failing tests.**
  - In `replacement_policy_limits.rs`, `pinned_policy_budget_blocks_before_selection` (Review Focus 7):
    - a predecessor whose pinned policy has `max_attempts 2`, and two charged failed attempts;
    - `assess_task_for_revision` returns `Blocked("replacement_policy_retry_budget_exhausted")`;
    - the same lineage with one attempt exempt (`owner_stop` row) returns `Replace`.

    Build the pinned launch with `common::profiled::fixture` and the launch helpers the `native_initial_grant` tests use.
  - In `native_offers.rs`:
    - `refused_successor_is_skipped_and_other_work_is_offered`:
      - a ready task T1 whose successor the pinned policy refuses, and a ready task T2 of lower priority;
      - the poll answers 200 and offers T2;
      - a second poll does not offer T1 either;
      - no response is 409.
    - `refused_poll_selection_records_no_decision`: a poll whose selection is refused (no fresh availability observation) leaves `SELECT count(*) FROM intelligence_decisions` unchanged.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^replacement_policy_limits::/)'` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::refused_/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS. Then run:
  - `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'`;
  - `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^(replacement_|profiled_results::)/)'`;
  - `timeout 900 cargo test -p aq-server --lib --locked producer::workflow::tests`.
- [ ] **Step 5: Commit.** Commit message: `fix(native): a refused successor is skipped, never fails the poll, and writes no decision row (spec 0007 Q1)`.

---

### S10: Per-boot registration receipt cap (aq-server) — Sonnet

Each slot attempt registers a new boot, so the lifetime cap of 1,024 receipts per node (`registration.rs:198-208`) would end a slot after 1,024 attempts. Make the cap per boot.

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/registration.rs:198-203`. The query becomes `SELECT count(*) FROM native_registration_receipts WHERE node_id=$1 AND response->'identity'->>'boot_id'=$2`, with `.bind(request.identity.boot_id.to_string())`.
- Test: `crates/aq-server/tests/it_native/native_registration.rs` (existing module)

- [ ] **Step 1: Write the failing test** `registration_receipt_cap_is_per_boot`:
  - insert 1,024 receipt rows for boot A with direct SQL (the table is append-only; insert only);
  - a Register for boot B succeeds;
  - a 1,025th distinct-operation Register for boot A is refused with `"native registration receipt capacity exhausted"`.
- [ ] **Step 2: Run it.** Run `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_registration::registration_receipt_cap_is_per_boot$/)'`. Expected: FAIL on the boot-B register.
- [ ] **Step 3: Implement** the one-line query change.
- [ ] **Step 4: Run it again.** Expected: PASS. Then run the whole `native_registration::` module.
- [ ] **Step 5: Commit.** Commit message: `fix(native): cap registration receipts per boot, not per node lifetime`.

---

### S11: Enrolled machines are allowed automatically (owner answer Q3) (aq-server) — Sonnet

Every production node is created by `store.rs` `redeem_enrollment` (`store.rs:875`), from a single-use code. "Enrolled and not revoked" is therefore the authority. Revocation is already enforced in three places:
- the bearer check (`worker_api.rs:121`);
- `lock_current_node` (`worker_api.rs:136-151`);
- the offer fence (`project_bindings.rs:590-631`).

A per-credential and a per-binding flag, each set once, let every enrolled node pass the static `allowed_node_ids` checks. No check needs a new database lookup.

**Files:**
- Modify: `crates/aq-intelligence-config/src/lib.rs` (`CredentialConfig`, ≈ L17-45)
- Modify these 11 checks, replacing `<x>.allowed_node_ids.contains(&n)` with `<x>.allows(n)`. Nothing else changes in them.

  | File | Line |
  | --- | --- |
  | `crates/aq-server/src/intelligence.rs` | 167 |
  | `crates/aq-server/src/intelligence/inventory.rs` | 212 |
  | `crates/aq-server/src/intelligence/resolver.rs` | 133 and 533 |
  | `crates/aq-server/src/profiled_execution/login_refresh/api.rs` | 208 |
  | `crates/aq-server/src/profiled_execution/material.rs` | 75 and 190 |
  | `crates/aq-server/src/profiled_execution/material/issuance.rs` | 68 |
  | `crates/aq-server/src/profiled_execution/presence/inventory.rs` | 101 |
  | `crates/aq-server/src/profiled_execution/phase_execution/current.rs` | ≈ 206-212 |
  | `crates/aq-server/src/profiled_execution/renewal/current.rs` | ≈ 82-88 |

- Modify: `crates/aq-server/src/intelligence/registry.rs:3-38` (digest `Placement`)
- Modify: `crates/aq-server/src/project_bindings/domain.rs` (`ProjectBinding` ≈ L47-73; validation ≈ L226; `eligible_node` ≈ L284)
- Modify: `crates/aq-server/src/notifications/sources.rs:89` and `:109` (two SQL strings)
- Modify: every `CredentialConfig { … }` and `ProjectBinding { … }` struct literal the compiler reports. Most are in `crates/aq-server/tests`.
- Test: unit tests in `aq-intelligence-config`, `registry.rs` and `project_bindings/domain.rs`; `crates/aq-server/tests/it_access/intelligence.rs`

**Changes, exactly:**
1. `CredentialConfig` gains `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub allow_enrolled_nodes: bool`, with a line in its hand-written `Debug`. Add:

   ```rust
   /// Spec 0007 Q3: listed nodes, or every enrolled node when the flag is set.
   /// Revoked nodes are refused by authentication and the offer fence, not here.
   pub fn allows(&self, node: Uuid) -> bool { self.allow_enrolled_nodes || self.allowed_node_ids.contains(&node) }
   ```

2. `registry.rs`: `Placement` gains a variant `Enrolled(&'a aq_selection::AuthorizedCredential, &'a [uuid::Uuid], aq_selection::CredentialKind, bool)`. Use it only when `c.allow_enrolled_nodes` is true; then the last element is `true`. Existing configurations keep their stored digest (`credential_legacy_default_config_digest_is_unchanged` stays green). Setting the flag changes the digest, so it needs one revision bump, a restart and `POST /api/intelligence/register`, once per credential.
3. `ProjectBinding` gains `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub allow_enrolled_nodes: bool`. An existing binding serializes byte-identically.
   - `eligible_node` returns `self.allow_enrolled_nodes || <existing expression>`.
   - The "no eligible worker identities" validation (≈ L226) also accepts `self.allow_enrolled_nodes`.
4. In both `notifications/sources.rs` SQL strings, the node-membership predicate gains `OR (b.binding->>'allow_enrolled_nodes')::boolean IS TRUE`.

- [ ] **Step 1: Write the failing tests.**
  - `aq-intelligence-config`: `allows_listed_or_enrolled_nodes`.
  - `registry.rs` tests: `enrolled_flag_changes_digest_only_when_set`. `legacy_config()` keeps `LEGACY_DIGEST`; with `allow_enrolled_nodes = true` the digest differs.
  - `project_bindings/domain.rs` tests:
    - `enrolled_flag_makes_every_node_eligible_and_validates_without_lists`;
    - `binding_without_flag_serializes_unchanged`, comparing against an existing test binding's JSON.
  - `it_access/intelligence.rs`:
    - `enrolled_node_inventory_accepted_with_flag`: a node that is not in `allowed_node_ids` reports inventory under a credential with the flag → accepted. Without the flag → 403.
    - `revoked_node_is_refused_even_with_flag`: after `/api/workers/{id}/revoke`, its inventory report → 401.
- [ ] **Step 2: Run them.** Run:
  - `timeout 900 cargo test -p aq-intelligence-config --locked`
  - `timeout 900 cargo test -p aq-server --lib --locked -- registry::tests project_bindings::domain::tests`
  - `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^intelligence::(enrolled|revoked)_/)'`

  Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS. Then run:
  - `timeout 900 cargo nextest run -p aq-server --test it_playbook -E 'test(/^project_bindings::/)'`;
  - `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^(native_offers|scoped_presence_inventory|subscription_refresh)::/)'`;
  - `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^notification_/)'`.
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): credentials and project bindings may allow every enrolled node (spec 0007 Q3)`.

---

### S12: Qualification renewal route and merged facts (aq-server) — stronger model

**Files:**
- Create: `crates/aq-context-renewal/{Cargo.toml,src/lib.rs}`. Its deps are `aq-protocol`, `serde_json`, `sha2`, `hex` and `thiserror`. It is pure, with no database.
- Modify: `crates/aq-server/src/profiled_execution/context_renewal.rs` (replaces the S4 stub)
- Modify: `crates/aq-server/src/machine_api.rs` (route `/v1/worker/qualifications`)
- Modify: `producer/workflow.rs:484-490` and `producer/build.rs:255-270`.
  - Use `context_renewal::current_facts(state, now).await?` instead of `config.context_qualifications`.
  - Replace `.map_err(|_| refused())` with a `warn` log of the `context_budget::Error` display text, then `refused()`.
- Modify: `crates/aq-server/src/dashboard_api/swarm.rs`. Build the `MeasuredWindow`s from `context_renewal::current_facts(state, now).await?` instead of `p.context_qualifications`, so a renewed node does not show `measurement_expired`.
- Modify: `protocol/commands/v1/routes.json` (one `node` entry)
- Test:
  - `crates/aq-context-renewal/src/lib.rs` unit tests;
  - `crates/aq-server/tests/it_native/native_qualification_renewal.rs` (a new module in `it_native/main.rs`; add it to the nextest `selection-fixture` filter);
  - `crates/aq-server/tests/it_dashboard/dashboard_swarm.rs`.

**Interfaces — Produces:**

```rust
// aq-context-renewal
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenewalError { #[error("call set")] CallSet, #[error("incomplete call {0}")] Incomplete(String),
    #[error("model {0}")] Model(String), #[error("context window")] Window, #[error("bound {0}")] Bound(String),
    #[error("prompt {0}")] Prompt(String), #[error("base fact shape")] BaseShape }
/// base: a Qualification as JSON; counting_body: the parsed body of base.counting_evidence (ms1-counting shape:
/// {"measurements":[{"call","prompt_utf8_bytes","prompt_sha256",...}], ...}).
/// Returns the base fact with observed_at_ms = effective_at_ms and expires_at_ms = effective_at_ms + 86_400_000.
pub fn renew(base: &serde_json::Value, counting_body: &serde_json::Value, submission: &aq_protocol::MeasurementSubmission,
    effective_at_ms: u64) -> Result<serde_json::Value, RenewalError>;

// context_renewal.rs (aq-server)
pub const EFFECTIVE_DELAY_MS: u64 = 60_000;
/// Valid renewals newest-first (observed_at_ms DESC, expires_at_ms > now), then the configured facts in order.
/// Configured facts only when the store format is below aq_machine_control::FORMAT.
pub async fn current_facts(state: &AppState, now_ms: u64) -> Result<Vec<Qualification>, AppError>;
/// The fact current_facts would select first among those with observed_at_ms <= now < expires_at_ms, or the newest
/// not-yet-effective renewal if one exists (so services can prepare the switch); None without profiled config.
pub async fn current_pin(state: &AppState, now_ms: u64) -> Result<Option<aq_protocol::QualificationPin>, AppError>;
pub async fn record(state: &AppState, node: &crate::worker_api::AuthenticatedNode,
    submission: &aq_protocol::MeasurementSubmission) -> Result<aq_protocol::QualificationPin, AppError>;
```

**Validation in `renew`, in this order.** Fail on the first failure.
1. The call names equal the set of `counting_body.measurements[].call` (normally `minimal`, `mixed` and `adversarial`), each exactly once. Otherwise `CallSet`.
2. Per call:
   - `prompt_utf8_bytes` and `prompt_sha256` equal the counting row. Otherwise `Prompt(call)`.
   - `result.type == "result"`, `result.subtype == "success"` and `result.is_error == false`. Otherwise `Incomplete(call)`.
3. `result.modelUsage` has exactly one key, and it equals `base.subject.model`. Otherwise `Model(key)`.
4. `modelUsage[model].contextWindow == base.capacity_tokens`. Otherwise `Window`.
5. `usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens ≤ prompt_utf8_bytes × base.maximum_tokens_per_utf8_byte + base.fixed_overhead_tokens`. Otherwise `Bound(call)`.

These are the checks `ms1-profiled.py` `measured()` makes (success, model `claude-sonnet-5-5`, a single context window, total input within the counting bound). That is the procedure of `docs/implementation/MS1.md` "Refreshing the context measurement" step 3, which `remeasure.sh` runs today.

**`record`:**
1. Find each configured fact whose credential allows the node: the credential is `[intelligence].credentials[*]` with `credential.profile == fact.subject.credential_profile`, and `credential.allows(node.id())` (S11).
2. Take the counting body from `config.profiled_execution.definition_bodies`, where `artifact() == fact.counting_evidence`, parsed with `serde_json::from_str(body())`.
3. Call `renew` with `effective_at_ms = now + EFFECTIVE_DELAY_MS`.
4. Deserialize the result into `Qualification`, `validate()` it, take `artifact()`, and insert it into `context_qualification_renewals` (`ON CONFLICT (renderer_digest) DO NOTHING`).
5. Return the pin of the first renewed fact.

Errors:
- no eligible fact → `AppError::Conflict("no configured qualification this node may renew")`;
- a `RenewalError` → `AppError::BadRequest(e.to_string())`.

- [ ] **Step 1: Write the failing tests.**
  - Unit tests in `aq-context-renewal`, with fixtures built in the test from the shapes of `~/.local/share/aq-ms1/measure/*.jsonl` (the files `remeasure.sh` installs):
    - `valid_submission_renews_with_new_window`;
    - `missing_or_duplicate_call_is_call_set`;
    - `changed_prompt_is_prompt_error`;
    - `other_model_or_window_refused`;
    - `over_bound_refused`;
    - `error_result_is_incomplete`.
  - In `it_native/native_qualification_renewal.rs`:
    - `renewal_extends_offers_past_config_expiry`:
      - a config fact expires at T, and a valid submission is recorded before T;
      - at T + 1 ms, the poll offer's `startup.renderer_qualification` equals the renewed artifact;
      - without the renewal, the same poll is refused.
    - `renewal_not_effective_before_delay`: right after `record`, offers still use the config fact.
    - `expired_fact_logs_warn_reason` (Review Focus 5): capture tracing at warn with the subscriber helper the it_native tests already use. The poll with no valid fact logs `"qualified context facts are unavailable, expired or ambiguous"`.
    - `node_outside_credential_cannot_renew`: a credential without `allow_enrolled_nodes`, and a node not in its list → 409.
    - `control_reports_qualification_pin`.
  - In `dashboard_swarm.rs`, `renewed_node_has_no_measurement_hold`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-context-renewal --locked` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_qualification_renewal::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** Keep `context_renewal.rs` at 150 lines or fewer.
- [ ] **Step 4: Run them again.** Expected: PASS. Then run:
  - `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'`;
  - `… --test it_profiled -E 'test(/^profiled_results::/)'`;
  - `… --test it_dashboard -E 'test(/^dashboard_swarm::/)'`.
- [ ] **Step 5: Commit.** Commit message: `feat(swarm): node-submitted context qualification renewals, merged into offer budgeting and the swarm read`.

---

### S13: Sticky owner availability with a dashboard switch (owner answer Q2) (aq-server) — stronger model

**Why.**
- Selection counts an availability observation only while `now - received_at_ms ≤ observation_max_age_ms` (`aq-selection/src/candidate_core.rs:108-118`; aq-ms1 uses 30 s).
- Only the owner posts positive observations (`intelligence.rs:115`).
- An always-on swarm therefore stops getting offers about 30 s after the owner's last post.

**Owner answer:** availability is sticky. It stays on until the owner switches it off, with a dashboard switch.

**The model.** A singleton switch, `owner_availability`, from migration 0101, has three states:

| State | Keeper | Offer gate | Manual `observe` |
| --- | --- | --- | --- |
| **Unset** (no row; the default) | Does nothing | None | Works as today |
| **On** | Keeps every owner `available` observation fresh | None | Still works |
| **Off** | Does nothing | `offers()` makes no new offer and runs no selection, so no decision row is written | Still accepted, but the gate holds |

**Files:**
- Create: `crates/aq-server/src/intelligence/keeper.rs` and `crates/aq-server/src/intelligence/availability_action.rs`. Declare both in `intelligence.rs`.
- Modify: `crates/aq-server/src/intelligence.rs`. Extract the body of `observe()` after its receipt lookup into `record_operator_observation` (below). `observe()` keeps exactly its current validation, receipts and responses.
- Modify: `crates/aq-server/src/lib.rs`. Spawn `intelligence::keeper::run(state.clone(), rx.clone())` next to `row_retention::run` (`lib.rs:489`).
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs`. In `offers()`, next to the S4 machine gate: when `has_format` and `owner_availability(..).state == Off`, log at info `"owner availability is off; no new offers"` and `return Ok(page)`.
- Modify: `crates/aq-server/src/dashboard_api/actions/existing.rs`. Register and add the schema of `intelligence::availability_action::SetAvailability`.
- Modify: `crates/aq-server/src/dashboard_api/service.rs`. Add `capability("swarm.availability.control", a.owner && state.config.intelligence.is_some(), "Owner availability requires an owner account and a configured intelligence registry")`.
- Modify: `crates/aq-dashboard-contract/src/swarm_contract.rs`. `SwarmServer` gains `pub owner_availability: OwnerAvailability`, defined below.
- Modify: `crates/aq-server/src/dashboard_api/swarm.rs`. Fill it from `aq_machine_control::owner_availability`, or `{Unset, "1", null}` when the format is absent.
- Regenerate:
  - `protocol/dashboard-read/v1/schema.json`, and add `"owner_availability":{"state":"unset","generation":"1","since":null}` to `fixtures/swarm.json` `server`;
  - `protocol/dashboard-actions/v1/schema.json`, with a new fixture `fixtures/request-swarm-availability-set.json`;
  - `protocol/commands/v1/commands.json`.

  Document the switch in both READMEs.
- Test: `crates/aq-server/tests/it_access/intelligence.rs` (existing); `crates/aq-server/tests/it_dashboard/dashboard_worker_actions.rs`; `crates/aq-server/tests/it_native/native_offers.rs`

**Interfaces — Produces:**

```rust
// intelligence.rs
/// The shared core of `observe()`: advisory lock 712009 (taken by the caller's tx), registry check, credential
/// and node checks, sequence fence, revision bump, quota, upsert. `sequence: None` means max(previous + 1, now_ms).
/// `expected_revision: Some(r)` refuses unless the registry revision is r (observe passes its request value).
pub(crate) async fn record_operator_observation(state: &AppState, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: Uuid, scope: ObservationScope, availability: Availability, ttl_ms: u64, cooldown_ms: u64,
    sequence: Option<u64>, expected_revision: Option<u64>) -> Result<AvailabilityObservation>;

// keeper.rs
pub const KEEPER_INTERVAL: Duration = Duration::from_secs(10);
pub const REFRESH_AFTER_MS: u64 = 15_000;
pub async fn run(state: AppState, stop: watch::Receiver<bool>);
pub async fn refresh_once(state: &AppState, now_ms: u64) -> Result<u32, AppError>;

// availability_action.rs
#[derive(Clone, Serialize, Deserialize, JsonSchema)] #[serde(deny_unknown_fields)]
pub(crate) struct AvailabilityChange { pub operation_id: Uuid, pub available: bool,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))] pub expected_generation: String }
pub(crate) struct SetAvailability;   // KIND "swarm.availability.set", GRANT "swarm.availability", TARGET Subject
pub(crate) fn register(r: &mut Registry); pub(crate) fn schemas(r: &mut Registry);

// aq-dashboard-contract
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum OwnerAvailabilityState { Unset, On, Off }
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)] #[serde(deny_unknown_fields)]
pub struct OwnerAvailability { pub state: OwnerAvailabilityState,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))] pub generation: String, pub since: Option<Timestamp> }
```

**`refresh_once`:**
- Return `Ok(0)` unless the format is present and the switch is **On**.
- Otherwise, in one transaction with `SELECT pg_advisory_xact_lock(712009)`:
  1. Select the rows of `intelligence_observations` whose `observation->'source'->>'kind' = 'operator'`, whose `observation->>'state' = 'available'`, and whose `(observation->>'received_at_ms')::bigint < now - REFRESH_AFTER_MS`.
  2. Skip a row when any unexpired row with the same `scope` (ignoring `node_id`) has a source kind other than `operator` and a state other than `available`. The keeper never masks negative feedback.
  3. Otherwise rewrite the row with:
     - `sequence = old + 1` and a new `observation_id`;
     - `received_at_ms = now` and `expires_at_ms = now + (old.expires_at_ms - old.received_at_ms)`;
     - `cooldown_until_ms = now`;
     - `revision = registry.revision + 1`.

     Use the same `INSERT … ON CONFLICT` and `UPDATE intelligence_registry` statements as `observe()` (`intelligence.rs:216-224`).
  4. Return the number of rows refreshed.

**`SetAvailability`:**
- **`authorize`:**
  - `existing::owner`;
  - subject `{type:"swarm", id:"owner-availability"}` with no revision and empty `expected_versions`;
  - `operation_id` equal to the payload's;
  - `state.config.intelligence.is_some()`, else `ApiError::for_status(StatusCode::CONFLICT)`;
  - returns `Scope::AllAuthorized`.
- **`execute`:** one transaction.
  1. `set_owner_availability(available, expected, user.id, operation_id, now)`.
  2. When `available`, take advisory lock 712009 and call `record_operator_observation` once for each **enabled** credential of the loaded registry, with:
     - scope `{credential_revision, provider, credential_profile, account_reference, endpoint_reference}` from the credential, `model: None`, `node_id: None`;
     - `Availability::Available`, `ttl_ms 3_600_000`, `cooldown_ms 0`, `sequence None`, `expected_revision None`.
  3. Commit.
  - Outcomes: confirmed with receipt `owner_availability` at the new generation and affected `swarm`/`owner-availability` at the new generation. A CAS conflict → `Outcome::conflict({"owner_availability": generation})`. Any other error → `existing::failure(AppError::Internal)`.
- **`reconcile`:** the `owner_availability` row whose `operation_id` matches → confirmed.

**Known race (accepted):**
- A keeper rewrite between a poll's selection snapshot and its offer fence makes that one poll refuse with `"relevant availability changed"`. The slot backs off and retries.
- The fence holds `FOR SHARE` on the observations, so the window is only between the snapshot and the fence.
- Record this in `docs/implementation/0007-P1-server.md` (S14).

- [ ] **Step 1: Write the failing tests.**
  - In `it_access/intelligence.rs`:
    - `keeper_refreshes_owner_available_observation` (Review Focus 9): with the switch **On**, an owner observation 20 s old gets a new `received_at_ms == now` and a sequence one higher.
    - `keeper_is_idle_when_unset_or_off`.
    - `keeper_never_overrides_negative_feedback`: a worker `quota_unavailable` observation for the same scope → the owner row is left as it is.
    - `keeper_ignores_owner_unavailable`.
    - `observe_route_is_unchanged`: the existing `observe` tests in this module still pass. Run them all.
  - In `dashboard_worker_actions.rs`:
    - `availability_on_records_an_operator_observation_per_enabled_credential`;
    - `availability_stale_generation_is_a_conflict`;
    - `swarm_read_reports_owner_availability`: `unset` → `on` at generation 2 → `off` at generation 3;
    - `bootstrap_lists_availability_capability`.
  - In `native_offers.rs`, `owner_availability_off_makes_no_offer_and_no_decision` (Review Focus 9): with the switch Off, the poll is 200 with zero offers and `intelligence_decisions` is unchanged. Turning it On (through the action) and polling again gives an offer, with no manual observation.
- [ ] **Step 2: Run them.** Run:
  - `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^intelligence::/)'`
  - `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_worker_actions::(availability|swarm_read|bootstrap_lists_availability)/)'`
  - `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::owner_availability/)'`

  Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate the protocol files and run both `check.py` scripts and both `--check` examples.
- [ ] **Step 4: Run them again.** Expected: PASS. Run the `commands::` check too.
- [ ] **Step 5: Commit.** Commit message: `feat(intelligence): owner availability stays on until switched off, with the swarm.availability.set action (spec 0007 Q2)`.

---

### S14: Close the aq-server PR (aq-server) — Sonnet

- [ ] **Step 1: Run the full checks.**
  - `timeout 2400 cargo nextest run --workspace --locked --no-fail-fast` (PASS)
  - `timeout 900 cargo test --workspace --locked --doc` (PASS)
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --check`
  - `python3 protocol/dashboard-read/v1/check.py`, `python3 protocol/dashboard-actions/v1/check.py` and `python3 protocol/worker-machine/v1/check.py` (all PASS)
- [ ] **Step 2: Lower the budget.** Run `python3 scripts/build-budget.py --update`.
  - Expected: the aq-server ratchet goes **down** from 109,568 (the target is ≈ 109,150 or lower), and the test binary count stays 81.
  - If the count went up instead, move more code out before continuing. Never raise the ratchet.
- [ ] **Step 3: Write the docs.**
  - Add `docs/implementation/0007-P1-server.md`. It covers:
    - what was built;
    - the machine key model;
    - routes, tables (migration 0101), action kinds and capabilities;
    - the Q1/Q2/Q3 behaviours;
    - tests and limits, including the S13 race and the global 1,024-row `intelligence_decisions` cap;
    - the owner questions from this plan that are still open.
  - Add one line under `docs/specs/0007-swarm.md` §10's table: "Phase 1 server: see `docs/implementation/0007-P1-server.md`".
  - In `docs/implementation/MS1.md` "Known limits and owners", mark "Selection's 1-hour lineage budget refusal fails the poll instead of skipping the lineage" as fixed by S9. In "Refreshing the context measurement", add one sentence: "With the phase 1 service, nodes renew the fact themselves (`aq-worker service measure`); `remeasure.sh` stays for stacks without the service."
- [ ] **Step 4: Commit and open PR 2.** Commit message: `docs(swarm): phase 1 server notes; lower the lib ratchet`. The PR body lists:
  - migration 0101's tables;
  - the new routes, action kinds and capabilities;
  - the swarm read fields;
  - that it merges with aq-dashboard PR 3 after the D1 re-pin.

---

### D1: Re-pin the dashboard-read and dashboard-actions contracts (aq-dashboard, PR 3) — Sonnet

Follow commit `7b9be93` ("contracts: pin task-comment contracts …") as the model for dashboard-actions.

**Files:**
- `contracts/pinned/dashboard-read/**` and `contracts/pinned/source.json`: run `python3 scripts/pin-contract.py /home/jkern/dev/aq-platform/wt/swarm-p1-server dashboard-read <aq-server commit>`.
- `contracts/pinned/dashboard-actions/v1/{README.md,schema.json,check.py,fixtures/*}` and `contracts/live/dashboard-actions/{README.md,schema.json,check.py,fixtures/*}`:
  - copy them byte for byte from `git -C /home/jkern/dev/aq-platform/wt/swarm-p1-server archive <commit> protocol/dashboard-actions/v1 | tar -x -C <tmpdir>`;
  - update `contracts/live/dashboard-actions/source.json` (`commit` and every changed `sha256`);
  - update the `dashboard-actions/v1/*` `sha256` entries and `contract_commits["dashboard-actions"]` in `contracts/pinned/source.json`. Compute digests with `sha256sum`.
- `<aq-server commit>` is the PR 2 head while PR 2 is open. After PR 2 merges, repeat this task against the merge commit.
- Write the reminder in the commit message and PR body, not in a JSON file: "pinned to aq-server branch swarm/p1-server <sha>; re-pin to the merge commit before merging".

- [ ] **Step 1: Check the copies.** Run `python3 contracts/pinned/dashboard-read/v1/check.py`, `python3 contracts/pinned/dashboard-actions/v1/check.py` and `python3 contracts/live/dashboard-actions/check.py`. Expected: PASS.
- [ ] **Step 2: Run the tests.** Run `pnpm test` (PASS: the existing fixture-driven tests still pass, including the contract digest tests) and `pnpm typecheck`.
- [ ] **Step 3: Commit.** Commit message: `contracts: pin dashboard-read (swarm control) and dashboard-actions (worker and availability kinds) from aq-server swarm/p1-server`.

### D2: Swarm adapter — machine control and owner availability; bindings that allow enrolled machines (aq-dashboard) — Sonnet

**Files:**
- Modify: `src/api/wire/swarm.ts`. `SwarmMachine.control` and `SwarmServer.owner_availability`, matching S5 and S13.
- Modify: `src/api/adapters/swarm.ts` and `src/api/adapters/swarm.test.ts`
- Modify: `src/mocks/swarm.ts`.
  - `SWARM_MOCKUP`'s `desktop` machine gets `control: { desired: "running", generation: "1", since: null, pending_handoffs: 0 }`. Every other machine gets `control: null`.
  - `server.owner_availability` is `{ state: "on", generation: "2", since: <ago(3_600_000)> }`.
  - Export `SWARM_STOPPED`, a copy whose `desktop` machine has `state: "stopped"` and `control.desired: "stopped"`, `generation: "2"`, `pending_handoffs: 1`, with every node `service: { state: "stopped", since: … }`.
- Modify: `src/api/adapters/projectBindings.ts`, `projectBindings.test.ts` and `src/features/settings/ProjectBindings.tsx`

**Interfaces — Produces** (in `src/api/adapters/swarm.ts`):

```ts
export type MachineControlView = { desired: "running" | "draining" | "stopped"; generation: string; since: string | null; pendingHandoffs: number };
export type OwnerAvailabilityView = { state: "unset" | "on" | "off"; generation: string; since: string | null };
// MachineView gains:   control: MachineControlView | null
// SwarmView.server gains: ownerAvailability: OwnerAvailabilityView | null
```

**Rules:**
- **`control`:**
  - `null` when it is absent, `null`, or not a record. An older server has no field.
  - `null` also when `desired` is not one of the three values, or `generation` does not match `/^[1-9][0-9]{0,18}$/`. An unknown value never becomes a control.
  - `pendingHandoffs` is a number, or else 0.
- **`ownerAvailability`:** `null` when it is absent or invalid. An unknown `state` gives `null`.
- **Project bindings:**
  - The binding adapter accepts `allow_enrolled_nodes === true` as satisfying the "worker groups or node ids" check.
  - `ProjectBindingView` gains `enrolledNodes: boolean`.
  - `ProjectBindings.tsx` shows `Enrolled machines: allowed` under the node list when it is true.

- [ ] **Step 1: Write the failing tests:**
  - `swarm.test.ts`:
    - `adapts machine control`;
    - `unknown desired state gives no control`;
    - `missing control is null (older server)`;
    - `adapts owner availability`;
    - `unknown availability state is null`.
  - `projectBindings.test.ts`: `a binding that allows enrolled machines with no listed nodes adapts`.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t "control|availability|enrolled"`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again.** Expected: PASS, including every `src/features/agents/swarm/*.test.tsx`. Then run `pnpm typecheck`.
- [ ] **Step 5: Commit.** Commit message: `feat(agents): machine control and owner availability in the swarm view`.

### D3: Start / Stop / Drain / Revoke on the Swarm machine pane (aq-dashboard) — Sonnet

**Files:**
- Create: `src/features/agents/swarm/swarmActions.ts`, `src/features/agents/swarm/SwarmAction.tsx` and `src/features/agents/swarm/MachineControls.tsx`, plus `MachineControls.test.tsx`
- Modify: `src/features/agents/swarm/SwarmPane.tsx`:
  - `MachinePane` renders `<MachineControls m={m} />` instead of `<WorkerControls labels={["Start worker", "Stop worker", "Drain"]} />`;
  - `NodePane` keeps its three disabled buttons, with the note below.
- Create: `src/mocks/swarmActions.ts`, a POST `/api/v1/actions` handler for the `worker.*` and `swarm.availability.set` kinds, following the handler in `src/mocks/playbooks.ts:194`. Add it to `src/mocks/handlers.ts`.
- Modify: `src/mocks/scenarios.ts`. Add `swarm.worker.control`, `swarm.worker.enroll` and `swarm.availability.control` to `ACTION_CAPABILITIES`.

**Interfaces — Produces:**

```ts
export type SwarmActionKind = "worker.start" | "worker.stop" | "worker.drain" | "worker.revoke" | "swarm.availability.set";
export const workerLabels = { "worker.start": "Start worker", "worker.stop": "Stop worker", "worker.drain": "Drain", "worker.revoke": "Revoke machine" } as const;
export function freezeWorkerAction(kind: "worker.start" | "worker.stop" | "worker.drain", m: Pick<MachineView, "id" | "control">, reason?: string): FrozenAction;
// body: {operation_id, kind, subject:{type:"machine",id:m.id}, expected_versions:{}, ...(reason?{reason}:{}),
//        payload:{operation_id, machine:m.id, expected_generation:m.control!.generation}}
export function freezeRevoke(m: Pick<MachineView, "id">): FrozenAction;  // payload {operation_id, machine:m.id}
export function freezeAvailability(available: boolean, a: OwnerAvailabilityView): FrozenAction;
// subject {type:"swarm", id:"owner-availability"}; payload {operation_id, available, expected_generation:a.generation}
// Every FrozenAction has project undefined (scope all_authorized; adaptActionOperation accepts it).
export type SwarmActionProps = { label: string; capability: string; freeze: () => FrozenAction; blocked?: string;
  intro: ReactNode; confirm: string; pendingText: string; confirmedText: string };
export function SwarmAction(props: SwarmActionProps): JSX.Element;
export function MachineControls(props: { m: MachineView }): JSX.Element;
```

**`SwarmAction`** is `src/features/playbooks/PlaybookAction.tsx`'s `Action` with these changes, and nothing else:
- the capability is `bootstrap.capability("dashboard.actions")`, then `bootstrap.capability(props.capability)`;
- there are no typed fields; `build()` is replaced by `freeze()`;
- the key is `${principalId}:${authorizationRevision}:${label}`;
- on `confirmed`, it calls `queryClient.invalidateQueries({ predicate: q => q.queryKey[4] === "swarm" })`.

It keeps the dialog, the confirmation checkbox, the error mapping, "Check status", "Retry exact submission" and the unsettled re-check.

**`MachineControls`:**
- **Status line.** `<p role="status">` reads:
  - `Worker: running`;
  - `Worker: draining (N running)`, where N is the number of agents on the machine's nodes, or `Worker: drained` when N is 0;
  - `Worker: stopped`, or `Worker: stopped, handing off N` when `pendingHandoffs > 0`.
- **No control.** When `m.control === null`, every worker button is disabled, with the note "This machine's worker is not installed as a service, so it cannot be started or stopped here." Revoke is still offered only when `m.id` starts with `node:`.
- **Which buttons are enabled:**
  - **Start worker** when `desired !== "running"`;
  - **Stop worker** when `desired !== "stopped"`;
  - **Drain** when `desired === "running"`;
  - **Revoke machine** always.

  A disabled button carries `blocked` text: "Already running", "Already stopped" or "Only a running worker can drain".
- **Dialog intros and confirmations** (spec §7.2):
  - Stop worker:
    - intro: "Each running agent on {label} is stopped, its work is saved as a commit with a handoff note, and a fresh agent continues it from that commit. This machine takes no new work until you start it." Followed by "{N} running agent(s) will be handed off.";
    - confirm: "Stop {label} now".
  - Drain:
    - intro: "Running agents on {label} finish; no new work starts here.";
    - confirm: "Drain {label}".
  - Start worker:
    - intro: "{label} takes new work again. Handoffs already requested continue.";
    - confirm: "Start {label}".
  - Revoke machine:
    - intro: "Revoking removes the credentials of every worker node on {label}. Its service stops taking work, and the machine leaves the Swarm page. Stop it first so running agents hand off their work. To use it again, enroll it with a new code.";
    - confirm: "Revoke {label}".
- **When `swarm.worker.control` is disabled,** the existing note "Worker start, stop and drain are not available on this server yet." shows, as today.
- **NodePane note** when the capability is enabled: "Drain node, restart and logs are not available yet. Use the machine's controls." When it is disabled, keep the existing note.

- [ ] **Step 1: Write the failing tests** in `MachineControls.test.tsx` (MSW `setupServer`, `renderApp`, `stubReactFlowDom`, as in `SwarmPane.test.tsx`):
  - `stop posts worker.stop with the machine generation`: the request body equals the S6 fixture shape (`machine: "desktop"`, `expected_generation: "1"`) and carries the `X-AQ-Request: 1` header.
  - `start is disabled while running and enabled when stopped` (uses `SWARM_STOPPED`).
  - `status line shows handing-off count` (`Worker: stopped, handing off 1`).
  - `legacy machine has no worker controls`.
  - `disabled with the existing note without swarm.worker.control`.
  - `conflict shows reload guidance`.
  - `confirmation is required`.
  - `revoke posts worker.revoke after confirmation`.
  - `a confirmed action refetches the swarm`.

  Update `SwarmPane.test.tsx` "machine pane shows load source and limits". It now expects `Start worker` disabled with the title "Already running", not the "not available" note.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t "MachineControls|machine pane"`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again.** Expected: PASS. Then run `pnpm typecheck`, `pnpm lint` and `pnpm build` (the mock-leak check passes).
- [ ] **Step 5: Commit.** Commit message: `feat(agents): start, stop, drain and revoke a machine from the Swarm pane (spec 0007 §7.2)`.

### D4: Add machine (aq-dashboard) — Sonnet

**Files:**
- Create: `src/features/agents/swarm/AddMachine.tsx` and `AddMachine.test.tsx`
- Modify: `src/features/agents/swarm/SwarmPane.tsx`. `ServerPane` renders `<AddMachine />` after its actions.
- Modify: `src/features/settings/FrameworkStatus.tsx`. Render `<AddMachine />` below the worker node list, because spec §6 lists Add machine under Settings.

**Behaviour:**
- The capability is `swarm.worker.enroll`. Without it, render nothing.
- The button "Add machine" POSTs `/api/enrollments` with `credentials: "same-origin"` and headers `Accept: application/json` and `X-AQ-Request: 1`, and no body. The response is `{code, expires_at}` (201). Anything else shows "The enrollment code could not be created."
- The panel then shows:
  - A "Worker URL" text field, prefilled with `${location.protocol}//${location.hostname}:28081`. Port 28081 is the worker listener of the aq-ms1 stack; document that in a comment.
  - The command, in a copyable `<code>`: `packaging/service/aq-install.sh --server <Worker URL> --bundle ~/aq-bundle --slots 2`.

    Below it: "Run it from an aq-worker checkout on the new machine. ~/aq-bundle holds the runtime binaries, native.json and repositories.json (docs/operations/install-service.md). The machine is allowed automatically and appears on this page."
  - The code, in a separate copyable field, with the note "Paste this when the script asks for it. It works once and expires at {expires_at}."
- The code never goes into the command line.
- The code is held in component state only, never in storage or the URL.

- [ ] **Step 1: Write the failing tests:**
  - `creates a code and shows it apart from the command`;
  - `command never contains the code`;
  - `hidden without swarm.worker.enroll`;
  - `a failed request says so`.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t AddMachine`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again.** Expected: PASS. Then run `pnpm typecheck` and `pnpm build`.
- [ ] **Step 5: Commit.** Commit message: `feat(agents): add a machine with a single-use enrollment code`.

### D5: Owner availability switch on the Swarm server pane (aq-dashboard) — Sonnet

**Files:**
- Create: `src/features/agents/swarm/OwnerAvailability.tsx` and `OwnerAvailability.test.tsx`
- Modify: `src/features/agents/swarm/SwarmPane.tsx`. `ServerPane` adds `<dt>Owner availability</dt><dd>…</dd>` and renders `<OwnerAvailability a={view.server.ownerAvailability} />` after the facts.

**Behaviour:**
- The fact reads:
  - `On (kept on since {formatAge})` for state `on`;
  - `Off` for `off`;
  - `Not kept on` for `unset`;
  - `unknown` when `null`.
- The capability is `swarm.availability.control`. When it is disabled, show its reason and no button.
- The `SwarmAction` button, with `freezeAvailability`:
  - **Keep available**, when the state is not `on`:
    - intro: "AQ keeps your subscription available for new agents until you turn it off.";
    - confirm: "Keep my availability on".
  - **Turn off availability**, when it is `on`:
    - intro: "AQ stops starting new agents until you turn availability back on. Running agents continue.";
    - confirm: "Turn my availability off".

- [ ] **Step 1: Write the failing tests:**
  - `shows on with its age`;
  - `keep available posts swarm.availability.set true at the generation`;
  - `turn off posts false`;
  - `hidden button without capability shows the reason`;
  - `null availability shows unknown`.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t OwnerAvailability`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again.** Expected: PASS. Then run:
  - `pnpm typecheck`, `pnpm lint` and `pnpm build`;
  - `pnpm test:visual`, or a Playwright screenshot, of `/agents?select=<desktop machine>` and of the server pane, at 1440 and 390 wide, as `docs/design/visual-acceptance.md` asks. Attach the screenshots to the PR.
- [ ] **Step 5: Commit and open PR 3.** Commit message: `feat(agents): owner availability switch on the Swarm server pane (spec 0007 Q2)`. The PR body says it must merge together with aq-server PR 2, after the D1 re-pin to PR 2's merge commit.

---
### W0: Move the aq-worker binary out of the lib count (aq-worker, PR 4) — Sonnet

**Files:**
- Move (`git mv`): `crates/aq-worker/src/main.rs` → `crates/aq-worker/src/bin/aq-worker/main.rs`; `src/native_diagnostics.rs`, `src/native_fixture.rs`, `src/native_fixture/replay.rs` and `src/fixture_responses_cli.rs` → `src/bin/aq-worker/` (keeping `native_fixture/replay.rs` as a subdirectory).
- Modify: `crates/aq-worker/Cargo.toml`. Add:

  ```toml
  [[bin]]
  name = "aq-worker"
  path = "src/bin/aq-worker/main.rs"
  ```

- [ ] **Step 1: Build.** Run `cd /home/jkern/dev/aq-platform/wt/swarm-p1-worker && CARGO_TARGET_DIR=/home/jkern/dev/aq-platform/target-swarm-p1-worker cargo build --locked -p aq-worker --bins`. Expected: success.
- [ ] **Step 2: Run the tests.**
  - `timeout 900 cargo test -p aq-worker --locked --all-features --bin aq-worker` (the CLI unit tests in main.rs pass)
  - `timeout 900 cargo test -p aq-worker --locked --all-features --test native_cli --test controller_cli` (PASS)
- [ ] **Step 3: Check the budget.** Run `python3 scripts/build-budget.py`. Expected: the aq-worker lib reads 70,483 (906 below 71,389). Do not `--update` yet (W12 does).
- [ ] **Step 4: Commit.** Commit message `build(worker): move the aq-worker binary and its bin-only modules under src/bin (they are not lib code)`.

### W2: `run_until_attempt` — one registration, poll until an offer (aq-worker) — Sonnet

**Files:**
- Modify: `crates/aq-worker/src/native/runner.rs`
- Modify: `crates/aq-worker/src/native/runner/recovery.rs` (`retained_attempts`)
- Test: `crates/aq-worker/src/native/runner/tests.rs` (existing in-crate tests)

**Interfaces — Produces:**

```rust
pub const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
#[derive(Debug, PartialEq, Eq)]
pub enum RunDisposition { ShutdownBeforeAdmission, NoOffer, AttemptStopped, PollingPaused } // PollingPaused is new
pub enum PollLoop<T> { Offer(T), Paused, Shutdown }
/// Pure loop: call `poll` until it yields Some, sleeping `interval` between calls; check `pause` and
/// `shutdown` before every call and during the sleep. An Err from `poll` is returned at once.
pub async fn poll_until_offer<T, F, Fut>(poll: F, interval: Duration, shutdown: &mut watch::Receiver<bool>,
    pause: &mut watch::Receiver<bool>) -> Result<PollLoop<T>>
where F: FnMut() -> Fut, Fut: Future<Output = Result<Option<T>>>;
/// Same admission path as run_once, but after one Register it republishes the scoped-presence inventory
/// and polls (limit 1) until an offer, `pause` (→ PollingPaused) or `shutdown` (→ ShutdownBeforeAdmission).
pub async fn run_until_attempt<E: NativeExecutor>(worker: &WorkerConfig, config: NativeConfig, executor: &mut E,
    shutdown: watch::Receiver<bool>, pause: watch::Receiver<bool>) -> Result<RunDisposition>;
// recovery.rs
/// Attempt ids in an existing state dir's journal (empty when no journal exists). Takes the state-dir lock.
pub fn retained_attempts(config: &NativeConfig, node: Uuid) -> Result<Vec<Uuid>>;
```

**Implementation.**
- Refactor the body of `run_once` from `let available = pipeline.available().await?;` through the `page.page.offers.pop()` into a closure. The closure:
  1. calls `heartbeat.check()?`;
  2. runs `producer.begin_inventory(config.inventory_ttl_ms)?`, then `producer.publish_inventory(&inventory).await?`, then `pipeline.record_inventory_sequence(..)`, then the `boot_ns < deadline` check;
  3. polls;
  4. returns `Some((page, inventory))` when there is an offer.
- `run_once` calls the closure once, as today, and keeps `NoOffer`.
- `run_until_attempt` wraps the closure in `poll_until_offer(…, IDLE_POLL_INTERVAL, …)`.
- Everything after the offer is shared code; no copy.
- The first inventory publication stays before the loop. Each iteration then begins a new inventory, so the 30 s TTL never lapses between polls.

- [ ] **Step 1: Write the failing tests** in `runner/tests.rs`:
  - `poll_until_offer_returns_the_third_result`: a `Cell` counter yields `None, None, Some(7)` → `Offer(7)` after 3 calls. Use `tokio::time::pause()` and an interval of 10 s.
  - `poll_until_offer_honours_pause_before_polling`: pause is already `true` → `Paused` with 0 calls.
  - `poll_until_offer_shutdown_during_sleep`: shutdown is sent during the sleep → `Shutdown`, after exactly 1 call.
  - `poll_until_offer_propagates_errors`.
  - `retained_attempts_is_empty_without_journal`: uses `config()` from tests.rs:20.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-worker --locked --all-features --lib native::runner::tests::poll_until_offer`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again** (PASS), plus `timeout 900 cargo test -p aq-worker --locked --all-features --lib native::runner` and `--test native_cli` (no regressions).
- [ ] **Step 5: Commit.** Commit message `feat(native): run_until_attempt polls under one registration until an offer`.

### W3: Driver handoff — stop, save and claim a handoff note (aq-worker) — Sonnet

**Files:**
- Modify: `crates/aq-worker/src/sandbox_driver.rs`
- Test: the `#[cfg(test)] mod tests` in the same file (≈ line 824)

**Interfaces — Produces:**

```rust
pub const HANDOFF_MARKER: &str = "handoff";   // file beside driver.json, written by the service
#[derive(serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct HandoffMarker { pub schema_version: u32, pub reason: aq_protocol::HandoffReason }
/// None when absent; None (and a println) when unreadable or invalid; never panics.
pub fn read_handoff_marker(path: &Path) -> Option<aq_protocol::HandoffReason>;
/// ≤ 500 chars after submit_claim's whitespace folding. Exact form:
/// "Handoff ({reason_words}): the agent was stopped before it finished; its work so far is saved. \
///  Changed: {first 20 changed paths joined by ", "}{" (+N more)" if more}. Last agent message: {≤200 chars}."
/// reason_words: owner_stop→"owner stop", service_stop→"service stop", restart→"node restart".
/// "Changed: none" when empty. The last-message clause is omitted when no assistant text exists
/// or when holds_secret(text, token) is true.
pub fn handoff_note(reason: aq_protocol::HandoffReason, changed: &[String], stream: &[u8], token: &[u8]) -> String;
fn last_assistant_text(stream: &[u8]) -> Option<String>; // last {"type":"assistant"} event: concatenated message.content[].text
```

**`drive()` changes** (≈ 742-817):
- Add a parameter `handoff: &Path`.
- In the 2 s loop, before `runner_exit`, run: `if let Some(reason) = read_handoff_marker(handoff) { handoff_reason = Some(reason); let _ = backend.stop(sandbox).await; break RunnerExit::Signalled; }`.
- After the loop, the **existing** copy-back code runs unchanged: transcript, `/aq/work` archive, `token_leak` check and `sync_tree`. A stopped container still serves `get_archive`.
- Return the handoff reason with `Drove`. Change `type Drove` to a struct with fields `rc`, `changed`, `stream`, `leak` and `handoff: Option<HandoffReason>`.

**`run()` changes:**
- Pass `&config_path.with_file_name(HANDOFF_MARKER)`.
- When `handoff` is `Some(reason)` and `leak` is `None`:
  1. stop and destroy the sandbox as on the normal path, since the work is now in the node worktree;
  2. `write_exit(config_path, false)`;
  3. `submit_claim(&config, handoff_note(reason, &changed, &stream, &token))`;
  4. `return Ok(4)`.
- The executor then sees `ResultRecorded`, and `driver_proven_cause(…, false)` makes it a Failure. The normal pipeline in `codex.rs:674-866` checkpoints, retains and reports the work. No executor change is needed.

- [ ] **Step 1: Write the failing tests:**
  - `handoff_marker_parses_and_ignores_garbage`: valid JSON → `Some(OwnerStop)`; `{"schema_version":2,…}` → `None`; missing file → `None`; non-JSON → `None`.
  - `handoff_note_lists_changes_and_last_message`: 25 paths → the first 20 and `(+5 more)`; the last assistant text is included; total `≤ 500`.
  - `handoff_note_never_carries_a_secret`: an assistant text containing the token → no "Last agent message" clause.
  - `handoff_note_without_changes_says_none`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-worker --locked --all-features --lib sandbox_driver::tests::handoff`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again** (PASS), plus the whole `sandbox_driver::tests` (no regressions).
- [ ] **Step 5: Commit.** Commit message `feat(driver): a handoff marker stops Claude, saves the workspace and claims a handoff note`.

### W4: Salvage an interrupted attempt after a restart (aq-worker) — stronger model

After a crash or reboot, the interrupted attempt must report a `Failed` result with its saved workspace, so that S7 starts the successor from it. Its handoff reason is `restart`, which S8 still charges to the lineage budgets.

**Files:**
- Modify: `crates/aq-worker/src/native/runner/recovery.rs`
- Possibly modify: `crates/aq-worker/src/native/journal.rs`. Only if the result-path pipeline methods refuse under `recovery_required()`; then add the narrowest gate described below.
- Test: `crates/aq-worker/src/native/runner/recovery/tests.rs` (existing)

**Interfaces — Produces:**

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum SalvageDisposition { AlreadyReported, NotStarted, Salvaged, Unsalvageable(&'static str) }
/// 1. reconcile_native(worker, config, attempt).  ResultRecorded → AlreadyReported; LaunchClosed → NotStarted.
/// 2. StopReported: find the attempt's stopped sandbox (backend.list_owned(node) filtered by attempt, state Stopped);
///    none → Unsalvageable("sandbox_absent").
/// 3. get_archive("/aq/work", 2 × max_workspace_bytes), read_tree; any entry for which
///    aq_agent_channel::holds_agent_credential(data) → Unsalvageable("credential_in_workspace").
/// 4. sync_tree into the attempt's workspace (the `workspace` field of host.root/driver-<launch>/driver.json).
/// 5. Produce the result claim handoff_note(Restart, &changed, &[], &[]) and run the same result steps as
///    codex.rs drive_running_owned after Stopped: retain_stopped_result(id, ResultStopCause::Failure) →
///    capture_result_source → transfer_result_source_step (bounded loop) → record_result_report.
/// 6. Leave the sandbox stopped (never destroy here; release stays an operator action).
pub async fn salvage_interrupted(worker: &WorkerConfig, config: &NativeConfig, attempt: Uuid)
    -> Result<SalvageDisposition>;
```

**Constraints for the implementer:**
- The claim must enter through the same durable result-claim intake the driver uses. That is the result-local channel under `state_dir/result`, written with `aq_agent_client::completion::ResultClient::open(dir)?.submit(..)`. That keeps `retain_stopped_result` unchanged.
- If the intake was closed by stop (`close_result_intake`), add a journal-recorded "salvage claim" event that is accepted only when all of these hold:
  - the attempt is physically stopped (a stop proof is retained);
  - no claim and no report intent exist;
  - the call comes from `salvage_interrupted`.

  Cover the journal format change with a `tests/journal_migration.rs` fixture (`tests/fixtures/journal-v4.rs`), following the v1–v3 pattern.
- Never invent a success. The outcome is always `Failed`.
- Repeated calls are safe. A second call after a recorded report returns `AlreadyReported` and posts nothing.
- The server accepts a result after Stopped under identity-only authority (`profiled_execution/results.rs:101-148`, `Policy::IdentityOnly`, `view.stopped` required). Do not touch the server for this.

- [ ] **Step 1: Write the failing tests** in `recovery/tests.rs`. Use the existing fixture journal builders, `FakeBackend` with a stopped sandbox, and the aq-git socket fake from `runner/sandbox/hydrate_tests.rs:286`:
  - `salvage_reports_failed_result_with_saved_workspace`: the report outcome is `Failed`, the conclusion starts `"Handoff (node restart)"`, and the source export contains the sandbox file.
  - `salvage_is_idempotent_after_report` (Review Focus 2).
  - `salvage_without_sandbox_is_unsalvageable`.
  - `salvage_refuses_workspace_holding_agent_credential`.
  - `salvage_of_unstarted_launch_is_not_started`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-worker --locked --all-features --lib native::runner::recovery`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again** (PASS), plus `--test journal_migration` and the `native::` lib tests (no regressions).
- [ ] **Step 5: Commit.** Commit message `feat(native): salvage an interrupted attempt's workspace as a failed handoff result after restart`.

### W5: Cloudflare Access on the Claude executor's relay (aq-worker) — Sonnet

**Files:**
- Modify: `crates/aq-terminal-bridge/src/http.rs`. `RelayHttp` gains `access: Option<(header::HeaderValue, header::HeaderValue)>`. `pub fn with_access(mut self, client_id: &str, client_secret: &str) -> Result<Self>` builds both values with `HeaderValue::from_str(..)?.set_sensitive(true)`. Every request builder adds `cf-access-client-id` and `cf-access-client-secret` when `access` is set. These are the header names `aq-agent-channel/src/lib.rs:166-167` uses.
- Modify: `crates/aq-worker/src/native/runner/claude_docker.rs:37-41`. Remove the `ensure!` that refuses CF credentials. After building `relay`, if both `cf_client_id` and `cf_client_secret` are `Some`, call `relay = relay.with_access(id, secret)?`. If exactly one is `Some`, bail with `"Access client id and secret must both be set"`.
- Test: `crates/aq-terminal-bridge/tests/http.rs` (existing); `crates/aq-worker/src/native/runner/claude_docker.rs` tests module

- [ ] **Step 1: Write the failing tests:**
  - `access_headers_reach_every_relay_request`: the existing test server records headers; every request carries both, and none carries them without `with_access`.
  - `claude_executor_accepts_access_enrollment_and_refuses_half`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-terminal-bridge --locked --test http access_headers` and `timeout 900 cargo test -p aq-worker --locked --all-features --lib claude_docker`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(relay): Access service-token headers so a Claude node can sit behind Cloudflare Access`.

### W6: `aq-node-service` — config, layout, server link and aq-git child (aq-worker) — Sonnet

**Files:**
- Create: `crates/aq-node-service/Cargo.toml`. Deps, all `workspace = true` where the workspace has them: `anyhow`, `aq-protocol`, `async-trait = "0.1"`, `fs2`, `libc`, `reqwest`, `serde`, `serde_json`, `sha2`, `thiserror`, `tokio`, `toml`, `tracing`, `uuid`, `zeroize = "1"`, `url = "2"`. `[dev-dependencies]`: `axum`, `tempfile`.
- Create: `crates/aq-node-service/src/{lib.rs,config.rs,layout.rs,link.rs,git.rs,enrollment_file.rs}`
- Modify: root `Cargo.toml` members; `crates/aq-worker/Cargo.toml` (`aq-node-service = { path = "../aq-node-service" }`)
- Test: unit tests in each module; `crates/aq-worker/tests/config.rs` (existing) for the enrollment-file compatibility test

**Interfaces — Produces:**

```rust
// config.rs  (service.toml, 0600, owner-only)
#[derive(Debug, Clone, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub schema_version: u32,                 // 1
    pub profile: String,                     // ^[a-z0-9][a-z0-9-]{0,31}$
    pub machine_key: String,                 // aq_protocol::valid_machine_key; claimed by slot 1 (S3)
    pub slots: u32,                          // 1..=MAX_MACHINE_SLOTS
    pub native_template: PathBuf,            // absolute; the pinned native.json (ms1-profiled.py output)
    pub state_root: PathBuf,                 // absolute
    pub aq_git: AqGitConfig,
    #[serde(default)] pub repositories: Vec<RepositoryBinding>, // ≤ 16
    #[serde(default)] pub measurement: MeasurementConfig,
    #[serde(default = "default_shutdown_grace_ms")] pub shutdown_grace_ms: u64, // 120_000; 10_000..=600_000
}
#[derive(Debug, Clone, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
pub struct AqGitConfig { pub binary: PathBuf, pub git_executable: PathBuf }      // both absolute
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)] #[serde(deny_unknown_fields)]
pub struct RepositoryBinding { pub repository_id: Uuid, pub identity: String, pub binding_generation: u64, pub object_format: String } // "sha1"
#[derive(Debug, Clone, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
pub struct MeasurementConfig {
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub token_file: Option<PathBuf>,           // 0600 regular file; required when enabled
    #[serde(default)] pub model: Option<String>,                 // e.g. "claude-sonnet-5-5"; required when enabled
    #[serde(default = "default_renew_before_ms")] pub renew_before_ms: u64, // 14_400_000; 3_600_000..=43_200_000
}
impl ServiceConfig { pub fn load(path: &Path) -> anyhow::Result<Self>; pub fn save(&self, path: &Path) -> anyhow::Result<()>; }

// layout.rs
pub struct Layout { pub config_dir: PathBuf, pub state_root: PathBuf }
impl Layout {
    pub fn for_profile(profile: &str) -> anyhow::Result<Self>; // config_dir = $XDG_CONFIG_HOME|~/.config /agent-q/service/<p>; state_root = $XDG_STATE_HOME|~/.local/state /agent-q/service/<p>
    pub fn service_toml(&self) -> PathBuf;                     // config_dir/service.toml
    pub fn slot_enrollment(&self, slot: u32) -> PathBuf;       // config_dir/slots/<slot>/worker.toml
    pub fn current(&self, slot: u32) -> PathBuf;               // state_root/slots/<slot>/current
    pub fn archive_dir(&self, slot: u32) -> PathBuf;           // state_root/slots/<slot>/archive
    pub fn renderer_pin(&self) -> PathBuf;                     // state_root/renderer.json
    pub fn machine_json(&self) -> PathBuf;                     // config_dir/machine.json (aq-machine-load MachineConfig)
}
/// Writes {"key","label","kind":"local","host_load_file"} (0600, create-new then rename); host_load_file is
/// "/mnt/c/ProgramData/agent-q/host-load.json" when `wsl`, else null. Label cut to 64 characters.
pub fn write_machine_json(path: &Path, key: &str, label: &str, wsl: bool) -> anyhow::Result<()>;
impl Layout {
    /// Creates current/{host,git} with mode 0700 (refuses if current exists).
    pub fn fresh_current(&self, slot: u32) -> anyhow::Result<PathBuf>;
    /// Wipes current/materials/*/material.bin, then renames current → archive/<UTC %Y%m%dT%H%M%SZ>-<4 hex>.
    /// Writes archive/<name>/EMPTY when `empty` (no attempts in the journal). Returns the archive path.
    pub fn archive_current(&self, slot: u32, empty: bool) -> anyhow::Result<Option<PathBuf>>;
    /// Deletes archives that contain EMPTY; keeps everything else.
    pub fn prune_empty(&self, slot: u32) -> anyhow::Result<u32>;
}

// enrollment_file.rs — byte-compatible with aq_worker::config::WorkerConfig
#[derive(Clone, Serialize, Deserialize)] #[serde(deny_unknown_fields)]
pub struct EnrollmentFile { pub server_url: String, pub node_id: Uuid, pub credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cf_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cf_client_secret: Option<String> }
impl EnrollmentFile { pub fn load(path: &Path) -> anyhow::Result<Self>; pub fn save(&self, path: &Path) -> anyhow::Result<()>; } // dir 0700, file 0600, create-new then rename
// Debug prints credential/cf_client_secret as "<redacted>".

// link.rs
pub struct ServerLink { /* origin: url::Url, credential: Zeroizing<String>, access: Option<(String, Zeroizing<String>)>, http: reqwest::Client (10 s timeout, no redirects) */ }
impl ServerLink {
    /// Refuses http:// unless host is 127.0.0.1 or ::1; refuses userinfo, query, fragment, non-root path.
    pub fn new(origin: &str, credential: &str, access: Option<(&str, &str)>) -> anyhow::Result<Self>;
    pub fn from_enrollment(file: &EnrollmentFile) -> anyhow::Result<Self>;
    pub async fn slots(&self, req: &SlotsRequest) -> anyhow::Result<SlotsResponse>;           // POST /v1/worker/slots
    pub async fn control(&self) -> anyhow::Result<ControlResponse>;                           // POST /v1/worker/control {"schema_version":1}
    pub async fn handoff(&self, notice: &HandoffNotice) -> anyhow::Result<()>;                // POST /v1/worker/handoffs
    pub async fn submit_measurement(&self, s: &MeasurementSubmission) -> anyhow::Result<QualificationPin>; // POST /v1/worker/qualifications
    pub async fn machine_report<T: serde::Serialize + Sync>(&self, report: &T) -> anyhow::Result<()>;    // POST /v1/worker/machine (phase 2 route)
}
/// The error of a non-2xx response; callers find it with `e.downcast_ref::<LinkError>()`.
#[derive(Debug, thiserror::Error)] #[error("server answered {status}{}", code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
pub struct LinkError { pub status: u16, pub code: Option<String> }
/// The aq-worker-ctl machine-config rule: lower-case, every byte outside [a-z0-9-] becomes '-', cut to 63.
/// None when the result is not a valid key (for example it starts with '-').
pub fn default_machine_key(hostname: &str) -> Option<String>;
// Every request: Authorization: Bearer <credential>; cf-access-client-id/-secret when access is set;
// Content-Type: application/json. A non-2xx response becomes a LinkError carrying status and the server's
// `error` code (ApiError body), never the request body or headers.

// git.rs — port of aq-server scripts/ms1-node-run.py (exchange/stop)
pub struct AqGit { /* child: tokio::process::Child, socket: PathBuf */ }
impl AqGit {
    /// Spawns `<binary> --state-dir <dir>/git --listen <dir>/git.sock --git-executable <git> serve`
    /// (stdin/stdout/stderr null, new process group), waits ≤ 10 s for the socket.
    pub async fn start(config: &AqGitConfig, state_dir: &Path) -> anyhow::Result<Self>;
    /// repo.register for each binding (expected_binding_generation 0; params.binding = {identity, mode:"worker",
    /// binding_generation, object_format}); refuses unless the reply has ok:true, the same request_id and mode "worker".
    pub async fn register(&self, repos: &[RepositoryBinding]) -> anyhow::Result<()>;
    /// SIGTERM, wait 10 s, then SIGKILL.
    pub async fn stop(self) -> anyhow::Result<()>;
}
```

- [ ] **Step 1: Write the failing unit tests:**
  - `config::tests`:
    - `loads_valid_and_refuses_bounds`: slots 0 and 9 are refused, a relative path is refused, a bad profile is refused, an invalid `machine_key` (`"Desk"`) is refused, and `measurement.enabled` without `token_file` or `model` is refused;
    - `defaults_fill_grace_and_renew_window`.
  - `layout::tests`:
    - `fresh_current_is_private_and_unique`: mode 0700; a second call fails while `current` exists;
    - `archive_wipes_material_and_marks_empty`;
    - `prune_deletes_only_empty_archives`;
    - `machine_json_is_a_machine_config`: the file `write_machine_json` writes loads with `aq_machine_load::MachineConfig::load_optional` (add `aq-machine-load = { path = "../aq-machine-load" }` to `[dev-dependencies]`) and keeps the key, the label and `host_load_file`; it is mode 0600.
  - `link::tests`:
    - `refuses_plain_http_off_loopback`: `http://10.0.0.5:8081` → Err; `http://127.0.0.1:28081` → Ok; `https://aq.example.com` → Ok;
    - `sends_bearer_and_access_headers`: an axum loopback server asserts the headers and the paths for the five calls;
    - `error_carries_status_not_secrets`: a 401 gives a `LinkError { status: 401, .. }`, and its display has no credential;
    - `default_machine_key_follows_the_ctl_rule`: `"DESKTOP-7Q"` → `"desktop-7q"`, `"my_box.local"` → `"my-box-local"`, `"-x"` → `None`.
  - `git::tests`:
    - `register_sends_repo_register_frames`: a fake aq-git, a tiny tokio `UnixListener` in the test answering `{"ok":true,"request_id":…,"result":{"mode":"worker"}}`; skip `start` and construct `AqGit` through a `#[cfg(test)]` constructor over an existing socket.
  - In `crates/aq-worker/tests/config.rs`:
    - `node_service_enrollment_file_is_a_worker_config`: `aq_node_service::EnrollmentFile::save`, then `aq_worker::config::WorkerConfig::load` reads the same node id and server URL.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-node-service --locked` and `timeout 900 cargo test -p aq-worker --locked --all-features --test config node_service`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(node-service): service config, state layout, server link and aq-git child`.

### W7: `aq-node-service` — the slot supervisor (aq-worker) — Sonnet

**Files:**
- Create: `crates/aq-node-service/src/supervisor.rs`
- Test: the unit tests in that file (fake backend, fake control)

**Interfaces — Produces:**

```rust
#[derive(Debug, Clone)]
pub struct SlotContext { pub slot: u32, pub node_id: Uuid, pub enrollment: PathBuf, pub state_dir: PathBuf,
    pub renderer: Option<ArtifactPin> }
#[derive(Debug, PartialEq, Eq)] pub enum AttemptOutcome { AttemptStopped, Paused, ShutdownBeforeAdmission }
#[derive(Debug, PartialEq, Eq)] pub enum RecoverOutcome { Clean, Reported, Salvaged, Held(String) }
#[async_trait::async_trait]
pub trait SlotBackend: Send + Sync + 'static {
    async fn run_attempt(&self, ctx: SlotContext, shutdown: watch::Receiver<bool>, pause: watch::Receiver<bool>)
        -> anyhow::Result<AttemptOutcome>;
    async fn recover(&self, ctx: SlotContext) -> anyhow::Result<RecoverOutcome>;
    fn retained_attempts(&self, ctx: &SlotContext) -> anyhow::Result<Vec<Uuid>>;
    async fn measure(&self, token_file: &Path, model: &str) -> anyhow::Result<MeasurementSubmission>;
}
#[async_trait::async_trait]
pub trait ControlSource: Send + Sync + 'static {
    async fn control(&self) -> anyhow::Result<ControlResponse>;
    async fn handoff(&self, notice: &HandoffNotice) -> anyhow::Result<()>;
    async fn submit_measurement(&self, s: &MeasurementSubmission) -> anyhow::Result<QualificationPin>;
}
#[async_trait::async_trait] impl ControlSource for ServerLink { /* forwards */ }

pub const CONTROL_INTERVAL: Duration = Duration::from_secs(5);
pub const QUIET_WINDOW: Duration = Duration::from_secs(30);
pub const BACKOFF_START: Duration = Duration::from_secs(30);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
pub struct Supervisor<B: SlotBackend, C: ControlSource> { /* config, layout, backend: Arc<B>, control: Arc<C>, git: Arc<dyn GitFactory> */ }
impl<B: SlotBackend, C: ControlSource> Supervisor<B, C> {
    pub fn new(config: ServiceConfig, layout: Layout, backend: Arc<B>, control: Arc<C>, git: Arc<dyn GitFactory>) -> Self;
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> anyhow::Result<()>;
}
#[async_trait::async_trait]
pub trait GitFactory: Send + Sync { async fn start(&self, state_dir: &Path) -> anyhow::Result<Box<dyn GitChild>>; }
#[async_trait::async_trait]
pub trait GitChild: Send { async fn register(&self, repos: &[RepositoryBinding]) -> anyhow::Result<()>; async fn stop(self: Box<Self>) -> anyhow::Result<()>; }
/// Writes <driver dir>/handoff (create_new, 0600, {"schema_version":1,"reason":...}) for the driver whose
/// driver.json has owner.attempt_id == attempt, searching every slot's current/host/driver-*/driver.json.
/// Returns true when written or already present.
pub fn mark_handoff(layout: &Layout, slots: u32, attempt: Uuid, reason: HandoffReason) -> anyhow::Result<bool>;
/// Attempt ids of drivers that are running: driver.json present, exit.json absent.
pub fn running_attempts(layout: &Layout, slots: u32) -> anyhow::Result<Vec<Uuid>>;
```

**Algorithm (`run`):**

```text
startup:
  for slot in 1..=slots:
    if layout.current(slot)/journal exists:
        git = factory.start(current); git.register(repos)
        out = backend.recover(ctx)            // W9: posts HandoffNotice(Restart) then salvage
        git.stop()
        match out { Held(why) => log error, mark slot disabled (no loop for it), keep current in place
                    _ => layout.archive_current(slot, backend.retained_attempts(ctx)?.is_empty()) }
    elif layout.current(slot) exists: archive_current(slot, true)
    layout.prune_empty(slot)
state: renderer pin = layout.renderer_pin() if present else None (the template's own renderer applies)
spawn control task (every CONTROL_INTERVAL):
    resp = control.control()  (errors: log warn, keep last state)
    revoked: three consecutive errors whose LinkError status is 401 → pause_tx.send(true) and log error
        "the server refused this machine's credential (revoked from the Swarm page?); all slots are paused.
         Uninstall the service or enroll the machine again"; from then on poll control every 60 s, and a
        successful response clears the state and sends pause false
    for id in resp.handoff: mark_handoff(layout, slots, id, OwnerStop)      // repeats until the driver dir exists
    if let Some(pin) = resp.qualification and pin.renderer != current pin:
        if now >= pin.effective_at_ms - QUIET_WINDOW: pause_tx.send(true)
        if now >= pin.effective_at_ms: current pin = pin; persist renderer.json; pause_tx.send(false)
    measurement: if config.measurement.enabled and (pin is None or pin.expires_at_ms - now <= renew_before_ms)
        and no measurement in flight and last attempt >= 10 min ago:
        spawn { s = backend.measure(token_file, model); p = control.submit_measurement(&s); log info with p.expires_at_ms }
spawn one loop per enabled slot:
    backoff = BACKOFF_START
    loop until stopping:
        dir = layout.fresh_current(slot); git = factory.start(dir); git.register(repos)
        r = backend.run_attempt(ctx{renderer: current pin}, slot_shutdown_rx, pause_rx)
        git.stop(); layout.archive_current(slot, backend.retained_attempts(ctx)?.is_empty()); layout.prune_empty(slot)
        match r { Ok(AttemptStopped) => backoff = BACKOFF_START,
                  Ok(Paused) => wait until pause is false (or stopping),
                  Ok(ShutdownBeforeAdmission) => break,
                  Err(e) => log warn "{e:#}"; sleep(backoff) (abort on stopping); backoff = min(backoff*2, BACKOFF_MAX) }
on shutdown signal:
    stopping = true; pause_tx.send(true)                      // no new polls
    for id in running_attempts(layout, slots): control.handoff(ServiceStop) (best effort); mark_handoff(ServiceStop)
    wait for every slot loop to end, at most shutdown_grace_ms
    slot_shutdown_tx.send(true); wait at most 20 s more; return Ok
```

- [ ] **Step 1: Write the failing tests.** Use `tokio::time::pause()`, a `FakeBackend` that records calls and has a scripted result per call, a `FakeControl` with scripted responses, and a `FakeGit` factory:
  - `starts_one_loop_per_slot_and_archives_each_attempt`: slots 2, backend returns `AttemptStopped` once then `ShutdownBeforeAdmission` → 2 archives per slot; the current dirs are gone.
  - `recovers_existing_current_before_new_work`: a pre-created `current/journal` gives `recover` once before the first `run_attempt`.
  - `held_recovery_disables_only_that_slot`.
  - `handoff_marker_written_when_driver_dir_appears_later` (Review Focus 1): control returns handoff X; the driver dir for X is created after the first tick → the marker exists after the second tick with reason `owner_stop`.
  - `renderer_switch_pauses_polling_in_quiet_window` (Review Focus 4): a pin with `effective_at = now + 40 s` → pause is false at +5 s and true at +15 s; after +40 s pause is false, the next `run_attempt` gets the new renderer, and `renderer.json` is written.
  - `failing_slot_backs_off_and_prunes_empty_dirs` (Review Focus 5): the backend errors 4 times → sleeps of 30, 60, 120 and 240 s, and after each failure no EMPTY archive remains.
  - `shutdown_hands_off_running_attempts_then_waits`: a running driver dir with no `exit.json` → `HandoffNotice(ServiceStop)` is posted, the marker is written, and the slot shutdown fires only after `shutdown_grace_ms` when the backend never returns.
  - `measurement_runs_when_pin_near_expiry`: a pin expiring in 3 h with `renew_before` 4 h → `measure` and `submit_measurement` are called once.
  - `revoked_credential_pauses_all_slots` (Review Focus 8): control answers `LinkError { status: 401 }` three times → pause is true, no `run_attempt` starts afterwards, and the next control poll waits 60 s.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-node-service --locked supervisor`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS. Run each test 20 times with `for i in $(seq 20); do timeout 900 cargo test -p aq-node-service --locked supervisor -q || break; done` (no flakes).
- [ ] **Step 5: Commit.** Commit message `feat(node-service): slot supervisor with handoff, renderer switch, backoff and graceful stop`.

### W8: `aq-node-service` — OS service layer, systemd user units (aq-worker) — Sonnet

**Files:**
- Create: `crates/aq-node-service/src/os_service.rs`

**Interfaces — Produces:**

```rust
pub struct UnitSpec { pub profile: String, pub aq_worker: PathBuf, pub service_toml: PathBuf, pub requires_sandbox_unit: bool }
#[derive(Debug, PartialEq, Eq)] pub enum ServiceState { Active, Inactive, Failed, NotInstalled, Other(String) }
pub trait ServiceManager {
    fn kind(&self) -> &'static str;                                 // "systemd-user"
    fn install(&self, spec: &UnitSpec) -> anyhow::Result<()>;       // write unit, daemon-reload, enable --now aq-node@<p>.service
    fn uninstall(&self, profile: &str) -> anyhow::Result<()>;       // disable --now, remove instance link (template stays if others use it)
    fn start(&self, profile: &str) -> anyhow::Result<()>;
    fn stop(&self, profile: &str) -> anyhow::Result<()>;
    fn status(&self, profile: &str) -> anyhow::Result<ServiceState>;
}
pub trait HostProbe { fn read(&self, path: &str) -> Option<String>; fn run(&self, argv: &[&str]) -> anyhow::Result<(i32, String)>; }
pub struct RealHost;
#[derive(Debug)] pub struct Preflight { pub wsl: bool, pub linger: bool }
/// Linux only. Err when PID 1 (/proc/1/comm) is not "systemd"; on WSL (/proc/sys/kernel/osrelease contains
/// "microsoft", case-insensitive) the error says: "WSL2 needs systemd: add\n[boot]\nsystemd=true\nto /etc/wsl.conf,
/// then run `wsl --shutdown` from Windows". Err when `systemctl --user is-system-running` exits with 1 and prints
/// "offline". linger = `loginctl show-user $USER -p Linger --value` == "yes".
pub fn preflight(host: &dyn HostProbe) -> anyhow::Result<Preflight>;
pub struct SystemdUser<H: HostProbe> { pub unit_dir: PathBuf, pub host: H }
pub fn render_unit(spec: &UnitSpec) -> String;
/// cfg(target_os = "linux") → SystemdUser; cfg(target_os = "macos") → bail!("launchd service is spec 0007 phase 5")
pub fn detect() -> anyhow::Result<Box<dyn ServiceManager>>;
```

`render_unit` output, exactly. The template unit is `~/.config/systemd/user/aq-node@.service`, and `%i` is the profile:

```ini
# AQ worker service (spec 0007 §7.2). Installed by `aq-worker service install`.
[Unit]
Description=AQ worker service (%i)
Requires=aq-sandbox-podman.service
After=aq-sandbox-podman.service network-online.target
StartLimitIntervalSec=0

[Service]
Type=exec
ExecStart=<aq_worker> run --service-config %h/.config/agent-q/service/%i/service.toml
Restart=always
RestartSec=10
KillMode=mixed
TimeoutStopSec=150
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```

- `<aq_worker>` is replaced by `spec.aq_worker`.
- When `requires_sandbox_unit` is false, the `Requires=`/`After=` lines name only `network-online.target`.
- `KillMode=mixed` sends SIGTERM to `aq-worker` only, so it can hand off before systemd kills the children. `TimeoutStopSec=150` exceeds the default grace (120 s) plus 20 s.

- [ ] **Step 1: Write the failing tests:**
  - `unit_renders_exactly` (golden string above);
  - `preflight_explains_wsl_without_systemd`;
  - `preflight_reports_linger`;
  - `install_runs_daemon_reload_then_enable_now`: the fake host records argv `["systemctl","--user","daemon-reload"]`, then `["systemctl","--user","enable","--now","aq-node@local.service"]`;
  - `status_maps_is_active_output`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-node-service --locked os_service`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(node-service): systemd user service layer (macOS launchd reserved for phase 5)`.

### W9: CLI glue — `run --service-config`, `service …`, `machine.json` and machine reporting (aq-worker) — Sonnet

**Files:**
- Modify: `crates/aq-worker/src/bin/aq-worker/main.rs`
- Create: `crates/aq-worker/src/bin/aq-worker/service.rs`
- Test: `crates/aq-worker/tests/controller_cli.rs` (existing)

**CLI changes (clap):**

```rust
/// Own node identity, capacity and sandbox lifecycle until stopped.
Run {
    /// Legacy B5b controller configuration (admission stays fenced).
    #[arg(long, conflicts_with = "service_config", required_unless_present = "service_config")]
    controller_config: Option<PathBuf>,
    /// Spec 0007 service: multi-slot docker-local work from service.toml.
    #[arg(long)]
    service_config: Option<PathBuf>,
},
/// Install, enroll and inspect the worker service (spec 0007 phase 1).
Service { #[command(subcommand)] command: ServiceCmd },

enum ServiceCmd {
    /// Enroll slot 1 with a single-use code (prompted, or the first stdin line), claim the machine key,
    /// mint slots 2..N, write machine.json and service.toml.
    /// With --cf-service-token the next two prompts/lines are the node Access client id and secret.
    Enroll { #[arg(long)] profile: String, #[arg(long)] server: String, #[arg(long, default_value_t = 1)] slots: u32,
             #[arg(long)] name: Option<String>, #[arg(long)] machine_key: Option<String>, #[arg(long)] cf_service_token: bool,
             #[arg(long)] native_template: PathBuf, #[arg(long)] aq_git: PathBuf,
             #[arg(long, default_value = "/usr/bin/git")] git_executable: PathBuf,
             /// JSON array of RepositoryBinding.
             #[arg(long)] repositories: PathBuf,
             #[arg(long)] measurement_token_file: Option<PathBuf>,
             #[arg(long, requires = "measurement_token_file")] measurement_model: Option<String> },
    Install { #[arg(long)] profile: String },
    Uninstall { #[arg(long)] profile: String, #[arg(long)] purge: bool },
    Status { #[arg(long)] profile: String },
    /// Run the qualification measurement now and submit it.
    Measure { #[arg(long)] profile: String },
}
```

**`service.rs` interfaces:**

```rust
pub struct NativeSlotBackend { template: PathBuf }
#[async_trait::async_trait] impl aq_node_service::SlotBackend for NativeSlotBackend {
    // run_attempt: WorkerConfig::load(ctx.enrollment); let mut cfg = NativeConfig::load(&template)?;
    //   cfg.state_dir = ctx.state_dir; cfg.git_socket = ctx.state_dir.join("git.sock"); cfg.git_root = ctx.state_dir.join("git");
    //   if let Some(ExecutorConfig::ClaudeDockerLocalResultLocal{settings,..}) = &mut cfg.executor { settings.host.root = ctx.state_dir.join("host");
    //       if let Some(pin) = &ctx.renderer { settings.host.renderer = to_artifact_ref(pin)?; } }
    //   if let Some(pin) = &ctx.renderer { cfg.renderer = to_artifact_ref(pin)?; }
    //   cfg.validate()?; let mut ex = ClaudeDockerExecutor::configured(&worker)?;
    //   map run_until_attempt(..): AttemptStopped→AttemptStopped, PollingPaused→Paused, ShutdownBeforeAdmission|NoOffer→ShutdownBeforeAdmission.
    // recover: for id in retained_attempts(cfg, node): link.handoff(Restart) (best effort), then salvage_interrupted(..):
    //   AlreadyReported|NotStarted→Reported, Salvaged→Salvaged, Unsalvageable(w)→Held(w); none → Clean.
    // retained_attempts: aq_worker::native::runner::retained_attempts.
    // measure: crate::measure::run(&self.template, token_file, model) (W10; until then bail!("measurement not built")).
}
fn to_artifact_ref(pin: &ArtifactPin) -> anyhow::Result<aq_admission::ArtifactRef>; // via serde_json round trip
pub async fn enroll(cmd: ServiceEnrollArgs) -> anyhow::Result<()>;
pub async fn run_service(service_config: &Path) -> anyhow::Result<()>;
```

**`enroll` sequence:**
1. **Machine key.** `--machine-key`, else `aq_node_service::default_machine_key(hostname)`. If neither gives a valid key, stop with "pass --machine-key: the host name does not make a valid machine key". This runs before any network call.
2. **Secrets.** Read them with `read_secrets_from`: the code, plus the CF id and secret if `--cf-service-token`. `--runner-access-token` is not offered (owner question 6).
3. **Slot 1.** `Client::new(&server, None, cf…).enroll(EnrollRequest{ name: name or hostname, … })` gives slot 1. Save it with `EnrollmentFile::save(layout.slot_enrollment(1))`.
4. **Claim and mint, always, even with `--slots 1`.** Call `ServerLink::from_enrollment(slot1).slots(&SlotsRequest{schema_version:1, machine_key, slots})`.
   - A 409 (the key is in use) is an error: "machine key <k> is in use by another machine; revoke it from the Swarm page or pass --machine-key".
   - Save each slot that has a credential as an `EnrollmentFile`, with the same `server_url` and cf values.
   - A slot without a credential (minted earlier, then lost) is an error: "slot N exists on the server but its credential is not on this machine; revoke the machine from the Swarm page and enroll again".
5. **`machine.json`.** Call `aq_node_service::write_machine_json(&layout.machine_json(), &machine_key, label, wsl)`, which writes (0600) `{"key": <machine_key>, "label": <name or hostname, cut to 64 chars>, "kind": "local", "host_load_file": <h>}`.
   - On WSL (`/proc/sys/kernel/osrelease` contains `microsoft`), `<h>` is `"/mnt/c/ProgramData/agent-q/host-load.json"`: the service is host-native (`docs/operations/install-worker.md`, "Windows host load (WSL2)"). Otherwise it is `null`.
   - This is the same format `aq-worker-ctl machine-config` writes, and `aq_machine_load::MachineConfig::load_optional` must accept it.
6. Copy `native_template` to `config_dir/native.json` (0600).
7. Write `service.toml`, including `machine_key`.
8. Print `machine <key>: slots <node ids…>`. Never print secrets.

**`run_service`:**
1. Load the config.
2. Use slot 1's `ServerLink` as the `ControlSource`.
3. Spawn the machine report loop, the phase 2 behaviour for the service:
   - `aq_machine_load::sampler_from_file(&layout.machine_json(), layout.state_root.clone())`;
   - when that gives `Some((sampler, interval))`, run `tokio::spawn(aq_machine_load::report_loop(sampler, interval, |r| async move { slot1_link.machine_report(&r).await.map_err(|e| e.to_string()) }, <shutdown future>))`.

   Only slot 1 reports. The server groups the sibling slots by the server-held key (S5).
4. Build `Supervisor` with `NativeSlotBackend`.
5. Run it with the `shutdown_signal()` watch (SIGTERM or ctrl-c). Abort the report loop when it returns.

Add `aq-machine-load = { path = "../aq-machine-load" }` to `crates/aq-worker/Cargo.toml` if it is not already a dependency of the binary (phase 2 added it).

- [ ] **Step 1: Write the failing tests** in `tests/controller_cli.rs`:
  - `run_requires_exactly_one_config_kind`: `run` alone fails; `run --controller-config x --service-config y` fails; `run --service-config /nonexistent` exits 2 with the path in stderr.
  - `service_enroll_never_accepts_secrets_in_argv`: `--code` is not a flag (parse error).
  - `service_enroll_refuses_an_invalid_machine_key_before_any_request`: `--machine-key Desk --server http://127.0.0.1:9` exits non-zero with "machine key" in stderr, without connecting.
  - W6's `layout::tests::machine_json_is_a_machine_config` covers the `machine.json` format; this task only calls `write_machine_json`.
  - `service_status_without_install_reports_not_installed`: set `XDG_CONFIG_HOME` to a tempdir on the child process only (`Command::env`, never `std::env::set_var`).
  - Keep `run_reports_missing_worker_config_and_malformed_controller_config` passing.
  - Update `main.rs`'s `run_requires_explicit_controller_configuration` to accept either flag.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-worker --locked --all-features --test controller_cli` and `--bin aq-worker`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(worker): aq-worker run --service-config, service enroll/install/status/measure, machine.json and machine reports`.


### W10: Measurement run and prompts (aq-worker) — stronger model

The service's measurement must reproduce exactly the procedure that the aq-ms1 stack runs today with `~/.local/share/aq-ms1/remeasure.sh`, as documented in aq-server `docs/implementation/MS1.md`, "Refreshing the context measurement". The server (S12) then validates it with the same checks as `ms1-profiled.py` `measured()`.

**Files:**
- Create: `packaging/measure/{minimal,mixed,adversarial}.txt`.
  - Copy them from `~/.local/share/aq-ms1/measure/*.txt`.
  - Their SHA-256 must equal the `prompt_sha256` rows in the active `aq.ms1-counting` Definition. Verify that with `python3 -c` against `~/.local/share/aq-ms1/catalog-rev2.json`.
- Create: `crates/aq-node-service/src/measure.rs`: `pub const PROMPTS: [(&str, &[u8]); 3]`, using `include_bytes!` of the three files.
- Create: `crates/aq-worker/src/bin/aq-worker/measure.rs`: `pub async fn run(template: &Path, token_file: &Path, model: &str) -> anyhow::Result<MeasurementSubmission>`.

**`measure::run` contract.** These are the facts of `remeasure.sh` (commit `cfd9cbbf`); keep them exactly:
1. **Template.** Load the native template. Take:
   - `settings.host.docker_local` (the socket);
   - `settings.host.template` (the pinned Claude image digest, the same image as `worker/node.json` `docker_local.templates[0]`);
   - `settings.host.claude_executable`, which is `/opt/aq/bin/claude` in the template.
2. **Sandbox.** Call `DockerLocalBackend::connect_unix` and create one sandbox with:
   - owner `{ node_id: slot-1 node, boot_id: Uuid::new_v4(), attempt_id: Uuid::new_v4() }`;
   - resources `{cpu_millis: 1000, memory_bytes: 2 GiB, pids: 512}`, matching `--pids-limit 512`;
   - a hard deadline of now + 15 min;
   - no agent channel.
3. **Inputs.** `put_archive` `/aq/in/{minimal,mixed,adversarial}.txt` (mode 0644) and `/aq/in/run.sh`. The script is `remeasure.sh`'s `run.sh` with only its paths changed (`/p/` → `/aq/in/` for inputs, `/aq/out/` for outputs). It:
   - reads the token from `/aq/private/material` and deletes the file before the first call;
   - sets the environment `HOME=CLAUDE_CONFIG_DIR=/home/agent/.aq/claude`, `DISABLE_AUTOUPDATER=1`, `DISABLE_TELEMETRY=1`, `DISABLE_ERROR_REPORTING=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` and `PATH=/opt/aq/bin:/usr/local/bin:/usr/bin:/bin`;
   - runs `mkdir -p "$HOME" /aq/work && cd /aq/work`;
   - writes `$HOME/settings.json` = `{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"enableAllProjectMcpServers":false,"cleanupPeriodDays":1,"forceLoginMethod":"claudeai"}`;
   - writes `date +%s%3N > /aq/out/observed_at_ms` just before the calls;
   - runs, for each call:

     ```sh
     CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 /opt/aq/bin/claude -p --output-format stream-json --verbose --input-format text --model <model> --permission-mode bypassPermissions --setting-sources user --settings "$HOME/settings.json" --strict-mcp-config --no-session-persistence --disable-slash-commands < /aq/in/<call>.txt > /aq/out/<call>.jsonl 2>/aq/out/<call>.err
     ```

   `<model>` is the `model` argument, from `service.toml` `measurement.model`. For aq-ms1 it is `claude-sonnet-5-5`, the `MODEL` in `scripts/ms1-profiled.py`. The server refuses any other model (S12 rule 3).
4. **Token.** Deliver it with `deliver_private_material`: the bytes from the 0600 token file, parsed with `sandbox_driver::parse_token`. It becomes `CLAUDE_CODE_OAUTH_TOKEN` of each claude process only. It is never in argv, a container's env or config, a copied file or a log. Then `start_runner`, wait for `runner_exit` (poll every 2 s, deadline 10 min), and `get_archive` `/aq/out`.
5. **Clean up.** Stop and destroy the sandbox, with destroy authorization `operator_released(sandbox, "aq-swarm-measurement")`.
6. **Leak check.** Refuse if any `.jsonl` or `.err` file `holds_token`.
7. **Result.** Build the `MeasuredCall`s from the last `{"type":"result"}` event of each file.

Unlike `remeasure.sh`, this run needs no server recreation and no revision bump: the server stores the renewal (S12). So it may run while slots are working. The supervisor (W7) schedules it at most once per 10 minutes, and only near expiry.

- [ ] **Step 1: Write the failing test.** `measure::tests::prompts_match_the_counting_definition` (in `aq-node-service`) asserts the three prompt SHA-256 values, hard-coded from the counting Definition.
- [ ] **Step 2: Check the run against the existing validator.**
  - Run `aq-worker service measure --profile <p> --output <tmpdir>` once against the aq-ms1 stack. Add a hidden `--output` flag that writes the three `.jsonl` files and `observed_at_ms` and does not submit.
  - Then run `ms1-profiled.py` `measured()` on that directory, exactly as `remeasure.sh`'s "validate" step does (`AQ_MS1_HOME=<tmpdir>`). Every bound must pass.
- [ ] **Step 3: Do a live check, by hand and recorded.** Run `aq-worker service measure --profile local` (after W11 installs) and record the server's response pin in `docs/implementation/0007-P1-evidence/measure.json`.
- [ ] **Step 4: Commit.** Commit message: `feat(node-service): docker-local qualification measurement (the remeasure.sh procedure) and renewal submission`.

### W11: Install script and operator guide (aq-worker) — Sonnet

**Files:**
- Create: `packaging/service/aq-install.sh` (bash, `set -euo pipefail`)
- Create: `packaging/service/tests/test_install.py`. Use pytest in `unittest` style, like `packaging/worker/tests/test_worker_ctl.py`. It uses `shellcheck` when present, and a fake `PATH` with stub `systemctl`, `loginctl`, `hostname` and `aq-worker`.
- Create: `docs/operations/install-service.md`

**Script interface:**

```
aq-install.sh --server URL --bundle DIR [--profile NAME=local] [--slots N=1] [--name NAME] [--machine-key KEY]
              [--cf-service-token] [--native-template FILE=DIR/native.json] [--repositories FILE=DIR/repositories.json]
              [--measurement-token-file FILE --measurement-model MODEL] [--sandbox-bundle podman-linux-amd64.tar.gz]
```

**Steps, in order (spec §7.3):**
1. **Preflight.**
   - `uname -s` must be Linux, else print "Only Linux and WSL2 are supported." and exit 2.
   - If `/proc/sys/kernel/osrelease` contains `microsoft` and `ps -p 1 -o comm=` is not `systemd`, print the `/etc/wsl.conf` instructions and exit 3.
   - `systemctl --user show-environment` must succeed.
   - If `loginctl show-user "$USER" -p Linger --value` is not `yes`, print `sudo loginctl enable-linger $USER` and continue.
   - When `--machine-key` is given, it must match `^[a-z0-9][a-z0-9-]{0,62}$`, else exit 2.
2. **Sandbox runtime.** If `$XDG_RUNTIME_DIR/aq-sandbox/podman.sock` is missing:
   - with `--sandbox-bundle`, run `scripts/sandbox-daemon/aq-sandbox-podman.sh install <bundle>` and then `systemctl --user enable --now aq-sandbox-podman.service`;
   - without it, exit 4 with that instruction.
3. **Binaries.** `install -D -m 0755` `aq-worker`, `aq-session-host`, `aq-agent`, `aq-git` and `aq-sandbox-driver` from `--bundle` into `~/.local/share/agent-q/runtime/`, and symlink `~/.local/bin/aq-worker`.
4. **Enroll.** Run `aq-worker service enroll --profile P --server URL --slots N [--name] [--machine-key] [--cf-service-token] --native-template … --aq-git ~/.local/share/agent-q/runtime/aq-git --repositories … [--measurement-…]`. It prompts for the code itself; secrets never pass through this script. It writes `machine.json`.
5. **Install and start.** Run `aq-worker service install --profile P`.
6. **Measure.** If `--measurement-token-file` was given, run `aq-worker service measure --profile P`.
7. **Windows host load.** On WSL, if `/mnt/c/ProgramData/agent-q/host-load.json` does not exist, print (do **not** run) the probe instruction from `docs/operations/install-worker.md`: `powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows\aq-host-load.ps1 -Install -StartNow`.
8. **Status.** Print `aq-worker service status --profile P`, then: "This machine is allowed automatically on credentials and projects that allow enrolled machines (allow_enrolled_nodes). Start, stop, drain or revoke it from its pane on the Swarm page."

- [ ] **Step 1: Write the failing tests:**
  - `refuses_wsl_without_systemd`;
  - `never_passes_secrets_in_argv`: the stub `aq-worker` records its argv; no argv contains the stdin code;
  - `installs_binaries_and_calls_enroll_install_measure_in_order`;
  - `defaults_template_and_repositories_from_the_bundle`;
  - `refuses_an_invalid_machine_key`;
  - `prints_but_never_runs_the_windows_probe`: no `powershell` stub is ever called;
  - `exits_4_without_sandbox_socket_or_bundle`.
- [ ] **Step 2: Run them.** Run `python3 -m pytest packaging/service/tests -q`. Expected: FAIL.
- [ ] **Step 3: Implement the script and write the guide.** The guide covers:
  - prerequisites, linking the existing `install-worker.md` sections on WSL2 autostart, `aq-wsl-autostart.ps1` and "Windows host load (WSL2)";
  - what the bundle directory holds: the five binaries, `native.json` and `repositories.json`, copied from an enrolled machine of the same build, because `native.json` pins exact digests;
  - install, the machine key, and the Access token;
  - start/stop/drain/revoke semantics and which handoffs are charged to the budgets (Q1);
  - restart behaviour;
  - upgrade: `install -m 0755` the new bundle, then `systemctl --user restart aq-node@P`;
  - uninstall, and re-enrolling after a revoke.
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message: `feat(packaging): one-script service install with single-use enrollment (spec 0007 §7.3)`.

### W12: Close the aq-worker PR (aq-worker) — Sonnet

- [ ] **Step 1: Run the full checks.**
  - `timeout 2400 cargo test --workspace --locked --all-features` (PASS)
  - `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  - `cargo fmt --all -- --check`
  - `python3 scripts/package-admission.py --check`
  - `python3 protocol/pinned/worker-machine/v1/check.py` (unchanged contract, PASS)
- [ ] **Step 2: Lower the budget.** Run `python3 scripts/build-budget.py --update`. The aq-worker ratchet must go **down** from 71,389 (the target is ≈ 70,933 or lower), and the test binaries stay 105.
- [ ] **Step 3: Write the notes.** Add `docs/implementation/0007-P1-worker.md`, in the style of `0006-P3.md`.
- [ ] **Step 4: Commit and open PR 4.** Commit message: `docs(swarm): phase 1 worker notes; lower the lib ratchet`.

---

### W13: Terminal input for docker-local Claude attempts (aq-worker, PR 5) — stronger model

**Why.** Spec 0007 §5.4 (owner): clicking an agent opens its live terminal with control already taken. Phase 2 built that. But no live docker-local Claude attempt can ever be controlled, for three reasons:
- `validate_docker_local_manifest` (`crates/aq-worker/src/native/adapter/claude.rs:198-217`) refuses `m.terminal.input_enabled`.
- The server seals `input_enabled` from the adapter's reporting instruction (`producer/build.rs:643-646`, `result_startup::for_adapter`), and only the Codex interactive profile sets it.
- The aq-server terminal service refuses a control lease when input is disabled (`terminal/service.rs:729`).

The pane itself runs `aq-sandbox-driver`, which only polls container logs (`sandbox_driver.rs:742-817`, `logs_since` → `print_logs`). Claude runs in print mode, `-p --input-format text`, reading one prompt.

**Requirements:**
1. Typed input in the controlled pane reaches the agent.
2. The existing control-lease and terminal-relay security stays as it is:
   - only the lease holder's input is relayed;
   - only an attempt whose manifest seals `input_enabled = true` accepts input (`control_relay.rs:36-48`, `terminal_io.rs`);
   - the bounds `max_input_bytes 4096` and `max_pending_input_bytes 32768` hold.
3. The transcript and credential-leak guards stay intact:
   - `print_logs`'s token check on everything shown in the pane;
   - the `token_leak` check on the transcript;
   - the workspace credential scan on copy-back;
   - the token never in argv, env, files or logs.
4. The non-interactive recipe is unchanged and stays the default. Interactive is a separate, new recipe and profile.

**Design (the default; Step 1 confirms it before any code).** Keep print mode with streaming input instead of a TUI:
- **New profile and recipe**, following the Codex precedent `codex-0.166-chatgpt-interactive-result-local-v1` (`native/adapter/result_profile.rs`, `aq_codex_mapper::interactive_result_local_recipe_artifact`):
  - profile `claude-2.1.318-docker-local-interactive-result-local-v1`;
  - recipe artifact `aq.claude-docker-local-interactive-result-local-recipe` version 1, with digest = `Digest::of(body)` like `docker_local_recipe_body()`.
- **The recipe's argv** is the current one, with `--input-format stream-json` instead of `text`. The runner sends the prompt as the first stream-json user message, then keeps Claude's stdin open.
- **Input path:** pane → driver → container. The driver reads its own stdin, which the session host feeds from the leased relay, in canonical mode, one line at a time:
  - each line must be ≤ 4096 bytes and is wrapped as `{"type":"user","message":{"role":"user","content":[{"type":"text","text":<line>}]}}`;
  - it is delivered into the sandbox with `put_archive` as `/aq/in/messages/<seq>.json`;
  - the runner script forwards those files, in order, to Claude's stdin.

  No TTY and no container attach are needed, so the C2 fixed launch policy of `DockerLocalBackend` does not change.
- **Guards on input:**
  - a line that `holds_secret(line, token)` is refused, never forwarded, and the pane prints "Input refused: it contains a credential.";
  - pending input above 32,768 bytes is refused;
  - every forwarded line is echoed to the pane through `print_logs`.
- **End of input:** the runner closes Claude's stdin after the first `result` event when no message is waiting. After that, the pane prints "The agent has finished; input is closed."
- **Validation:** `validate_docker_local_manifest` accepts `input_enabled == true` only together with the interactive recipe, and the non-interactive recipe still refuses it.

**Owner gate.** If Step 1 shows that Claude 2.1.318 does not support streaming user messages in print mode, the only remaining design is the full TUI over a container TTY (`OpenStdin`/`Tty` in the create body, plus attach). That changes the fixed launch policy and drops the stream-json transcript, so usage and the result event come only from the result channel. **Stop and ask the owner (owner question 9)** before building it.

**Files:**
- Modify: `crates/aq-worker/src/native/adapter/claude.rs` (the interactive recipe body, argv and validation), `crates/aq-worker/src/native/adapter/result_profile.rs` (the new profile) and `crates/aq-worker/src/sandbox_driver.rs` (the input loop, the runner script and end of input).
- Do not modify: `crates/aq-session-host/**` and `crates/aq-worker/src/native/launch/control_relay.rs`. They already enforce the lease and the sealed `input_enabled`. If a change there seems needed, stop and report it.
- Docs: `docs/implementation/0007-P1-terminal.md` (design, probe evidence, guards).
- Test: `claude.rs` and `sandbox_driver.rs` unit tests; `crates/aq-worker/tests/native_cli.rs` or the existing sandbox runner test (`tests/support/sandbox_runner.rs`) for an end-to-end driver run with `FakeBackend`.

- [ ] **Step 1: Probe and decide.**
  - In the pinned Claude sandbox template (the image in `~/.local/share/aq-ms1/worker/node.json` `docker_local.templates[0]`), run the `remeasure.sh` procedure by hand. Use the `minimal` prompt sent as a stream-json user message with `--input-format stream-json`, then a second user message.
  - Record whether both turns produce `result` events and whether stdin EOF ends the process, in `docs/implementation/0007-P1-terminal.md`.
  - Continue only on the default design. Otherwise stop at the owner gate.
- [ ] **Step 2: Write the failing tests:**
  - `claude::tests::interactive_recipe_accepts_input_only_with_its_profile`;
  - `claude::tests::default_recipe_still_refuses_input`;
  - `sandbox_driver::tests::typed_line_becomes_a_stream_json_user_message`;
  - `sandbox_driver::tests::input_holding_the_token_is_refused_and_never_uploaded`;
  - `sandbox_driver::tests::pending_input_over_bound_is_refused`;
  - `sandbox_driver::tests::input_after_result_is_closed`;
  - `sandbox_driver::tests::pane_echo_passes_the_token_scan`.
- [ ] **Step 3: Run them.** Run `timeout 900 cargo test -p aq-worker --locked --all-features --lib -- claude::tests sandbox_driver::tests`. Expected: FAIL.
- [ ] **Step 4: Implement.** Keep the aq-worker lib within the W-table total: the cap is +200 lines. Run `python3 scripts/build-budget.py`.
- [ ] **Step 5: Run the tests again.** Expected: PASS. Then run the whole `sandbox_driver::tests`, `native::` lib tests and `--test native_cli` (no regressions).
- [ ] **Step 6: Commit, then close PR 5** with the W12 checks and `--update`. Commit messages:
  - `feat(driver): terminal input for docker-local Claude through streaming user messages (spec 0007 §5.4)`;
  - `docs(swarm): phase 1 terminal input notes; lower the lib ratchet`.

  The PR body gives the new recipe digest for S15.

### S15: Seal terminal input for the interactive Claude profile (aq-server, PR 6) — Sonnet

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/producer/result_startup.rs`.
  - Add `const CLAUDE_INTERACTIVE_PROFILE: &str = "claude-2.1.318-docker-local-interactive-result-local-v1";` and `const CLAUDE_INTERACTIVE_RECIPE_DIGEST: &str = "<digest from the PR 5 body>";`.
  - `interactive` becomes `opt.profile == INTERACTIVE_PROFILE || opt.profile == CLAUDE_INTERACTIVE_PROFILE`.
  - The Claude interactive profile requires its exact public recipe, as `interactive_profile_requires_its_exact_public_recipe_and_keeps_counted_startup` does for Codex.
- Test: the same file's `mod tests`; `crates/aq-server/tests/it_native/native_offers.rs`

- [ ] **Step 1: Write the failing tests:**
  - `claude_interactive_profile_seals_input_with_its_recipe`;
  - `claude_default_profile_keeps_input_disabled`;
  - `native_offers::interactive_claude_offer_seals_terminal_input`: the offer manifest has `terminal.input_enabled == true`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-server --lib --locked result_startup` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::interactive_claude/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** The cap is +15 lines in `crates/aq-server/src`. Then run the tests again. Expected: PASS.
- [ ] **Step 4: Commit.** Commit message: `feat(native): seal terminal input for the interactive docker-local Claude profile (spec 0007 §5.4)`.

---

### L0: Acceptance stack preparation (aq-server, PR 6) — Sonnet

**Files:**
- Modify: `scripts/ms1-profiled.py`:
  - The intelligence credential gains `"allow_enrolled_nodes": True` and the project binding `allow_enrolled_nodes=True`, unless env `AQ_MS1_ENROLLED_NODES=0`. `allowed` (the listed node ids) stays as it is.
  - When env `AQ_MS1_INTERACTIVE=1`, the profile is `claude-2.1.318-docker-local-interactive-result-local-v1` and the recipe definition is the interactive recipe body from the aq-worker checkout (`AQ_MS1_WORKER_SRC`). The default stays `PROFILE`.
- Modify: `scripts/ms1-demo.sh`. Add these commands:
  - `service-status`: prints `aq-worker service status --profile ms1` and the `/api/v1/swarm` machines' `id`, `state`, `control` and each node's `service`;
  - `handoffs`: psql `SELECT attempt_id, reason, requested_at, candidate_oid FROM attempt_handoffs ORDER BY requested_at DESC LIMIT 5`;
  - `renewals`: psql `SELECT renderer_digest, observed_at_ms, expires_at_ms, node_id FROM context_qualification_renewals ORDER BY observed_at_ms DESC LIMIT 5`;
  - `decisions`: psql `SELECT count(*) FROM intelligence_decisions`.
- Note for operators, in the commit message: the next `up.sh profiled` after this lands needs a bumped `AQ_MS1_REVISION`. `remeasure.sh`'s drift check needs `AQ_MS1_ALLOW_DRIFT=1` once, because the generated configuration changes.
- Test: `scripts/tests` has none for ms1. Instead:
  - check `python3 -c "import ast,sys; ast.parse(open('scripts/ms1-profiled.py').read())"`;
  - run `AQ_MS1_ENROLLED_NODES=0 python3 -I -c "…"` to import the module and assert that `build()`'s intelligence credential has no `allow_enrolled_nodes` key, and that with the default it is `True`.

- [ ] **Step 1: Implement and run the checks above.**
- [ ] **Step 2: Commit.** Commit message: `scripts(ms1): acceptance flags and commands for the swarm phase 1 service`.

### L: Live acceptance on the aq-ms1 stack — stronger model

**Stack:**
- `http://127.0.0.1:28080` (admin and dashboard) and `http://127.0.0.1:28081` (worker), through `~/.local/share/aq-ms1/up.sh`.
- The `rebuild-server`, `profiled` and `import-login` commands are documented in `scripts/ms1-demo.sh`.

**Evidence** goes in `docs/implementation/0007-P1-evidence/` (aq-server), with secret-free files only. Before committing, run `grep -rE 'aqn_|aqa_|aqe_|sk-ant' docs/implementation/0007-P1-evidence`. Expect no output.

**Preparation:**
1. **Server and dashboard.** Rebuild the server from aq-server main (PRs 2 and 6 merged), with `AQ_MS1_SERVER_SRC` pointing at a fresh worktree of main, then run `up.sh rebuild-server`. Rebuild the dashboard bundle from aq-dashboard main.
2. **Worker.** Rebuild aq-worker from main (PRs 4 and 5 merged) with `rebuild-worker.sh --driver`. The driver binary changed (W3, W13), so its digest changes.
3. **Profile.** Rerun `up.sh profiled` with a bumped `AQ_MS1_REVISION` and `AQ_MS1_INTERACTIVE=1`, so that `native.json` pins the new driver digest and the interactive recipe, and the credential and binding allow enrolled nodes.
4. **Register.** Run `POST /api/intelligence/register`, which `up.sh profiled` does, so that the credential digest with `allow_enrolled_nodes` is installed.
5. **Old nodes.**
   - Stop the old ms1 node with `up.sh node-stop`. It reports the key `desktop` in its `machine.json`; once the service holds `desktop`, the old node shows as `node:<uuid>`.
   - The E6a unit `aq-worker@local.service` points at another server (port 32839). Leave it as it is, but record `systemctl --user status aq-worker@local` in the evidence.
6. **Tasks.** Seed four fresh tasks with `python3 scripts/ms1-task.py`. Each prompt must keep Claude busy long enough to interrupt: "create files step1.txt … step6.txt, one per minute, running `sleep 60` between them, then publish".
7. **Bundle.** Prepare `~/aq-bundle` from `~/.local/share/aq-ms1/runtime` (the five binaries) plus `~/.local/share/aq-ms1/node/config/native.json` and `repositories.json` containing `[{"repository_id":"e0000000-0000-4000-8000-000000000003","identity":"e0-development","binding_generation":1,"object_format":"sha1"}]`.

**Step 1: Install the service on this WSL2 desktop (Q3).**
1. On the Swarm page, select the server and press **Add machine**. Take a screenshot with the code redacted.
2. Run, pasting the code at the prompt:

   ```sh
   packaging/service/aq-install.sh --server http://127.0.0.1:28081 --bundle ~/aq-bundle --profile ms1 --slots 2 \
     --machine-key desktop --measurement-token-file ~/.config/aq-secrets/claude-oauth-token --measurement-model claude-sonnet-5-5
   ```

3. Do **not** edit `allowed_node_ids` or the binding, and do not restart the server.
4. Turn on **Keep available** in the server pane (Q2). Do not run `ms1-profiled.py observe`.

Expected:
- `systemctl --user is-active aq-node@ms1` prints `active`.
- The Swarm page shows machine `desktop` with 2 nodes online, `Worker: running`, and live load.
- `aq-worker service measure` returned a pin. `up.sh renewals` lists it, and no node holds `measurement_expired`.
- Within one poll interval, two tasks start (two sandboxes; `docker -H unix://$XDG_RUNTIME_DIR/aq-sandbox/podman.sock ps`).

**Step 2: Control a live agent (spec §5.4).**
1. Select a running agent on the Swarm page.
2. Expect the terminal to open with control already taken.
3. Type a line, for example "Also write NOTE.txt saying hello".
4. Expect the line to appear in the pane, and the agent to act on it: `NOTE.txt` appears in the workspace or the final commit.
5. Press Reconnect. Expect no control acquire to be posted (the relay log shows no new lease), and control stays with you.
6. Open the same agent in a second browser. Expect it to take control, and the first to show "Another viewer took control. You are watching."

Record screenshots and the relay lease log lines.

**Step 3a: Reboot-equivalent, graceful.** While an agent has written `step2.txt`, run `systemctl --user restart aq-node@ms1`.

Expected:
- The journal shows `HandoffNotice(service_stop)` and a driver claim "Handoff (service stop)".
- The attempt reports `Failed`, and D3 records rework.
- An `attempt_handoffs` row exists with `candidate_oid` set (`up.sh handoffs`).
- After the service comes back, a fresh attempt for the same task starts. Its workspace already contains `step1.txt` and `step2.txt`; check `git show <candidate_oid> --stat` in the aq-git volume.
- The task thread has the comment "Handoff from attempt …", authored by the predecessor agent (fix round 1: not a system comment), with the label "Worker-reported conclusion:".

**Step 3b: Reboot-equivalent, crash.** Repeat with `systemctl --user kill -s KILL aq-node@ms1`. systemd restarts the unit.

Expected:
- On startup, the supervisor logs `recover` for the slot, and salvage gives `Salvaged`.
- The successor starts from the salvaged commit, with comment reason "restart".

**Step 4: Stop from the dashboard.** With an agent running, press **Stop worker** on the `desktop` pane and confirm.

Expected:
- Within 10 s, the slot's driver logs the handoff, and the attempt reports `Failed` with the handoff note.
- The pane shows `Worker: stopped, handing off 1`, then `Worker: stopped`.
- No new attempt starts on this machine. The successor stays unoffered, because no other machine exists in phase 1; record that this is expected.
- The service stays `active`, and the nodes stay online.

**Step 5: Start again.** Press **Start worker**.

Expected:
- `control.generation` is higher.
- The successor is offered to this machine and starts from the handoff commit.

**Step 6: Repeated owner stops do not exhaust the budget (Q1).** Stop and start the same task's machine three more times, each time while that task's attempt runs.

Expected:
- Each time a successor is offered. There is never `lineage_retry_budget_exhausted` or `replacement_policy_*_budget_exhausted` in the server log for this task.
- `up.sh decisions` grows by at most one per offered attempt (two selections per offer).
- The server log for this node's polls has no `"native offer admission requirements unavailable"`.
- The task completes and D1 publishes.

**Step 7: Availability stays on (Q2).** Leave the machine idle with one ready task for at least 5 minutes, posting no observation.

Expected:
- The task is offered: the switch's synthesized attestation (fix round 1, review C1; there is no keeper) keeps the owner available, and `intelligence_observations` gains no owner row.
- Then press **Turn off availability**. Within one poll no new offer is made, and `up.sh decisions` stays unchanged across several polls.
- Press **Keep available** again, and offers resume.

**Step 8: Revoke (Q3).** Stop the machine, then press **Revoke machine** and confirm.

Expected:
- Both nodes are revoked, and the machine leaves the Swarm page.
- The service journal shows "all slots are paused", and it does not restart-loop.
- Then run `aq-worker service uninstall --profile ms1 --purge`, create a new code, and rerun the Step 1 install command. The machine enrolls again under the key `desktop` with no configuration edit, and takes work.

**Step 9: The handoff successor end to end (S7; fix round 1, review I8).** No test fixture drives `successor_base`, publication and the initial grant together (the lane report's "Fix round 1", I8, says why), so this step checks the wiring live. Use the handoffs of Steps 3a and 4/5.

For each handoff:
1. `up.sh handoffs` shows the predecessor's row with `candidate_operation_id`, `candidate_oid` and `note_comment_id` all set.
2. The successor's export is a fresh export of the saved commit, not the predecessor's export:
   - its `source_bases` base is `candidate_oid`;
   - its `source_retention_handle` equals `candidate_operation_id`;
   - its export operation differs from the predecessor's.
   The server log has no `shared source requires released predecessor retention receipt`, so the 0053/0095 `profiled_replacement_source_guard` let it through.
3. The successor is accepted and granted: it has an initial-grant receipt, its hot status is `granted`, and its workspace holds the saved step files.
4. The note was posted before any later refusal and exactly once:
   - one task comment has the id `note_comment_id`, author kind `agent` and the predecessor's attempt id, with the body label "Worker-reported conclusion:";
   - its `created_at` is before the successor's offer time;
   - a poll for the successor that was refused or retried after the candidate was created (log line "handoff candidate not ready", a refused publication, or a backstop skip) left no second comment.
5. `git show <candidate_oid>` (in the aq-git volume) shows a commit message that starts with "AQ: handoff of task" and has a "worker-reported: " line.

Record the SQL rows, the comment JSON and the commit message in the evidence.

**Finally:**
1. Capture `up.sh status`, `up.sh service-status`, `up.sh handoffs`, `up.sh renewals` and `up.sh decisions`, the dashboard screenshots (desktop and phone), and the server log lines filtered by `attempt_id`.
2. Write `docs/implementation/0007-P1-acceptance.md` with each step's observed outcome, IDs and timestamps. Claim no durations as estimates.
3. Commit on aq-server main with the message `docs(swarm): phase 1 live acceptance on aq-ms1 [skip ci]`.

If any step fails, use superpowers:systematic-debugging. Record the defect with its owner task in the acceptance doc. Fix it in a follow-up PR on the owning repo, then rerun from the failed step.

---

## Owner questions (decide before or during execution)

Q1–Q3 are answered (ledger) and applied: S8, S9, S11, S13, D2–D5 and L Steps 6–8. Still open:

1. **The sandbox-minute budget.** Q1 named the retry budget and the lineage time limit. This plan also exempts owner and service handoffs from B4's 240 sandbox-minute budget, because each handoff otherwise charges a whole admitted window. Confirm, or say to keep sandbox minutes charged.
2. **Is `service_stop` owner-triggered?** This plan exempts it: a `systemctl stop`/`restart`, a reboot or a shutdown sends SIGTERM to the service. A crash (`restart`) is still charged. Confirm, or charge `service_stop` too.
3. **The one-time opt-in for Q3.** Automatic authority needs `allow_enrolled_nodes = true` once per credential (a revision bump, a restart and `register`) and once per project binding. After that, a new machine needs no edit and no restart. With the flag, every enrolled, non-revoked node may use that credential. Is the one-time opt-in acceptable, or should the flag default to on?
4. **The measurement credential.** The plan reads the owner's Claude token from a 0600 file on the measuring machine, as MS1 does today, to run the three qualification calls. The alternative is a new server-leased measurement material, which this plan does not include. Phase 4 VMs cannot use the file option.
5. **The single-desktop handoff.** With no other machine (VMs are phase 4), a stopped desktop's successor waits until Start. Is that acceptable for phase 1? Spec §1's gaming scenario fully works only after phase 4.
6. **The runner Access token.** `service enroll` stores only the node Access token. The B6b runner token (`runner-access.json`) is unused by docker-local. Confirm it is not needed until phase 4.
7. **Node-pane controls.** Phase 2 drew "Drain node", "Restart" and "View logs" on the node pane. This plan wires only the machine-level controls; those three stay disabled, with a note. Is that acceptable for phase 1?
8. **The decision table cap.** S9 stops refused poll selections from writing `intelligence_decisions` rows. Every offered attempt still writes one or two, and the table's global cap of 1,024 rows is never pruned (`resolver.rs:312-325`). An always-on swarm will reach it, and then all selection stops. A retention rule for decisions no launch references is a separate task. Should it be scheduled in this phase?
9. **Terminal input design (W13), only if W13 Step 1 fails.** If Claude 2.1.318 cannot take streaming user messages in print mode, the only design left is the full TUI over a container TTY. That changes the C2 fixed launch policy (stdin and TTY on the container) and loses the stream-json transcript, so per-attempt token usage and the last-message part of the handoff note go away. Approve that, or keep docker-local Claude watch-only?
10. **Owner input in transcripts (W13).** Lines the owner types into a controlled agent become part of its conversation, transcript and pane replay. A line holding the Claude token is refused, but other secrets the owner pastes are not detected. Acceptable?

Ledger rulings that still stand: install from a local bundle in phase 1; no ratchet raise (S0/W0 moves stay); phase 1 started only after phase 2's server PR merged.

---

## Alignment notes (revision 2 against revision 1)

- **Machine identity.**
  - `nodes.machine_id UUID` / `slot` became `nodes.machine_key TEXT` / `slot`, claimed by slot 1 through `/v1/worker/slots` (with `machine_key` in `SlotsRequest`).
  - `machine.json` (phase 2's format) is written by `service enroll`, and the server key wins in the swarm grouping (S5).
  - `machine_controls` is keyed by `machine_key`. `ControlResponse`/`SlotsResponse` carry `machine_key`.
- **Migration.** `0100_machine_control.sql` became `0101_machine_control.sql` (state format 101). It gains `owner_availability` and drops the `nodes_default_machine` trigger, which a key does not need.
- **Line budgets.** The ratchet is now 109,568 (zero headroom). S0 adds two moves (provider quota, playbook domain) for about −1,598, and every task has a cap. aq-worker is unchanged at 71,389; W0 still frees 906.
- **Dashboard.**
  - Revision 1's D2 (`/api/workers` adapter in Settings) and its `/api/workers` fields (old S5) are removed.
  - Machine control now flows through `GET /api/v1/swarm` (S5, D2).
  - D3 wires the phase 2 Swarm pane buttons behind `swarm.worker.control`. D4 puts Add machine on the Swarm server pane and in Settings. D5 is new (the Q2 switch).
- **Names.**
  - The capabilities `workers.control`/`workers.enroll` became `swarm.worker.control`/`swarm.worker.enroll`.
  - The action kinds keep phase 2's names. Their payload names the machine key (`machine`), not a UUID.
- **Owner answers.**
  - Q1 → S8 (exemption) and S9 (refused successor skipped, no decision burn); revision 1's "a handoff counts as an attempt" guardrail is gone.
  - Q2 → S13 (switch, keeper, gate) and D5.
  - Q3 → S11 (`allow_enrolled_nodes`), the `worker.revoke` action (S6), the revoked-service pause (W7), and the removal of `AQ_MS1_EXTRA_NODE_IDS` and the manual allow-list step.
- **Measurement.** W10 reuses `remeasure.sh`'s exact flags, environment, settings and pids limit. S12 reuses `measured()`'s checks and makes the swarm read use renewed facts.
- **New requirement (supervisor, during this revision).** W13, S15 and L Step 2: terminal input for docker-local Claude, so the owner can control a live agent.
- **Task renumbering:**

  | Revision 1 | Revision 2 |
  | --- | --- |
  | S5 | S6 (actions; the `/api/workers` part dropped); S5 is new |
  | S6 | S7 |
  | S7 | S10 |
  | S8 | S12 |
  | S9 | S13 |
  | S10 | S14 |

  S8, S9, S11, S15, D5 and W13 are new.
