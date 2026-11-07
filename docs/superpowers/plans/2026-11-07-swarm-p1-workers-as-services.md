# Swarm phase 1: workers as services — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A machine runs `aq-worker` as an always-on Linux/WSL2 systemd user service with several slots that take real docker-local Claude work, enrolled by an install script and a single-use code, started, stopped (save and hand off now) and drained from the dashboard, with a fresh attempt from saved work after a restart, and with the 24 h context qualification renewed automatically.

**Architecture:**
- **One slot is one enrolled node.** aq-server allows one attempt per node (`compute::busy_locked`) and one boot per node registration, so a machine with N slots enrolls N nodes grouped by a `machine_id`. The first node redeems the single-use code. It then mints its sibling slot nodes through a new node-authenticated route.
- **The service drives the existing one-shot path.** Each slot runs the proven `run-native` pipeline: a fresh state directory, a worker-side `aq-git` and `run_once`. `run_once` gains a mode that keeps polling under one registration until an offer arrives. The generic slot supervisor and the OS service layer live in a new crate, `aq-node-service`. The glue to the aq-worker library lives in the `aq-worker` binary, which moves to `src/bin/aq-worker/` so it is not counted against the lib line ratchet.
- **Control is server-side.**
  - The server stores a desired state per machine (running, draining or stopped) and gates offers on it.
  - "Stop" also records a handoff request for every attempt running on the machine. The service learns about these from `POST /v1/worker/control` and touches a marker file. The sandbox driver then stops Claude, copies the workspace back and claims a handoff note.
  - The normal result pipeline commits, retains and reports the work as `Failed`. D3 sends it to rework, and B4 replaces it.
  - A new server rule bases a handed-off attempt's successor on the predecessor's saved commit, not on the original base, and posts the handoff note on the task thread.

**Tech stack:** Rust 1.96.1 (edition 2024), tokio, axum 0.8, sqlx/PostgreSQL, reqwest (rustls), clap 4, serde/serde_json/toml; systemd user units; bash install script; React 19 + Vitest + MSW (dashboard).

**Spec:** `aq-server/docs/specs/0007-swarm.md` (§7.2, §7.3, §8, §10 phase 1). Also read: architecture `docs/architecture/0002-execution-and-recovery.md` §5 and §7, spec 0006 §5, aq-worker `docs/implementation/0006-P3.md`, aq-server `docs/implementation/MS1.md` and `scripts/ms1-demo.sh` / `scripts/ms1-node-run.py` / `scripts/ms1-profiled.py`.

## Global constraints

Every task's requirements include these.

- **No time estimates** in code comments, docs, commit messages or reports (owner rule).
- **Never use `git stash`.** Commit or discard explicitly.
- **Worktrees and target directories.** Each worktree has its own private target directory. Use exactly these:

  | Repo | Worktree | `CARGO_TARGET_DIR` |
  | --- | --- | --- |
  | aq-worker protocol PR | `/home/jkern/dev/aq-platform/wt/swarm-p1-protocol` (branch `swarm/p1-protocol`) | `/home/jkern/dev/aq-platform/target-swarm-p1-protocol` |
  | aq-worker service PR | `/home/jkern/dev/aq-platform/wt/swarm-p1-worker` (branch `swarm/p1-worker`) | `/home/jkern/dev/aq-platform/target-swarm-p1-worker` |
  | aq-server | `/home/jkern/dev/aq-platform/wt/swarm-p1-server` (branch `swarm/p1-server`) | `/home/jkern/dev/aq-platform/target-swarm-p1-server` |
  | aq-dashboard | `/home/jkern/dev/aq-platform/wt/swarm-p1-dashboard` (branch `swarm/p1-dashboard`) | n/a |

  Create each with `git -C <repo> fetch && git -C <repo> worktree add -b <branch> <path> origin/main`. Never share a target directory.
- **Every `cargo test` / `cargo nextest` runs under `timeout`** (`timeout 900 …`). Do not export `CARGO_INCREMENTAL=0`. Do not alternate `cargo build` and `cargo test` without a reason.
- **aq-server integration tests stay in the 8 binaries** (`it_profiled`, `it_native`, `it_phase`, `it_schema`, `it_playbook`, `it_dashboard`, `it_access`, `it_server`). No new `[[test]]` and no new `tests/*.rs` binary. A new test goes into an existing module or a new module file listed in that binary's `main.rs`.
- **aq-worker integration tests go into existing `tests/*.rs` binaries.** Do not add a `tests/*.rs` file. The test binary budget is 105. New crates may have `#[cfg(test)]` unit tests in `src/` (those are not test binaries).
- **Lib line ratchets only go down.**
  - aq-server lib: 109,593. aq-worker lib: 71,389.
  - New code goes in new crates.
  - Each PR's net lib change must be ≤ 0, using the offsets planned in tasks S0 and W0.
  - The last task of each PR runs `python3 scripts/build-budget.py --update` and commits the lowered value.
  - Never edit `build-budget.toml` upward.
- **Migrations are contiguous.** The current max is `0099`. This plan adds exactly one, `0100_machine_control.sql`, which ends with `UPDATE meta SET value='100' WHERE key='state_format_version';`.
- **Secrets** (enrollment code, node credential, Access id/secret, Claude token) are never put in argv, environment variables of child processes, logs or unit files. They are read from a TTY prompt or stdin lines, or from 0600 files.
- **Plaintext HTTP only to numeric loopback.** A node sends its credential over `http://` only when the host is `127.0.0.1` or `::1`. Any other origin must be `https://`.
- **Contracts.** The aq-server PR (dashboard action kinds, `/api/workers` fields) merges together with the aq-dashboard re-pin PR.
- **Commit messages** end with:

  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

- **Existing values pinned by this plan:**
  - B4 limits stay `MAX_ATTEMPTS = 3`, `MAX_LINEAGE_ELAPSED_MS = 24 h` and `MAX_SANDBOX_MS = 240 min` (`profiled_execution/replacement.rs:18-20`).
  - The qualification TTL cap stays 86,400,000 ms (`context_budget.rs:125-130`).
  - The heartbeat interval stays 15 s and the offline threshold 60 s.
  - `aq_protocol::MAX_MACHINE_SLOTS = 8`.

## Review Focus

These conditions are not exercised by the happy path. Each line names the test that pins it and the task that owns it.

1. **Stop pressed while an attempt is still preparing.** At that moment no driver directory exists yet. The marker must still be written once the driver directory appears, and the attempt must hand off rather than run to completion.
   - Test: `supervisor::tests::handoff_marker_written_when_driver_dir_appears_later` (W7).
2. **The service is killed between copy-back and report, or salvage runs twice.** Salvage must be idempotent: the second call reports `AlreadyReported` and never posts a second report.
   - Test: `native::runner::recovery::tests::salvage_is_idempotent_after_report` (W4).
3. **Stop pressed twice, or Start pressed while a handoff is in flight.** The generation compare-and-swap refuses a stale `expected_generation`. Handoff rows are not duplicated. Start never cancels a handoff that was already requested.
   - Tests: `worker_api::machine_control_generation_conflicts_and_idempotent_handoffs` (S4) and `dashboard_worker_actions::stale_generation_is_a_conflict` (S5).
4. **Qualification renewal while slots are polling** (the renderer switch). No offer is taken with a stale renderer pin.
   - Test: `supervisor::tests::renderer_switch_pauses_polling_in_quiet_window` (W7).
5. **Every poll is refused, for example with 409 for an expired qualification, an exhausted lineage budget or a held registration.**
   - The slot must back off exponentially (30 s up to 300 s) and must not hot-loop or burn state directories.
   - The server must log the qualification reason at `warn`.
   - Tests: `supervisor::tests::failing_slot_backs_off_and_prunes_empty_dirs` (W7) and `native_qualification_renewal::expired_fact_logs_warn_reason` (S8).

---

## File structure

### aq-worker (`/home/jkern/dev/aq-platform/aq-worker`, read from `origin/main`)

| Path | Status | Responsibility |
| --- | --- | --- |
| `crates/aq-protocol/src/lib.rs` | modify (W1) | Machine wire types (slots, control, handoffs, qualification renewal) |
| `crates/aq-protocol/fixtures/machine-v1.json` | create (W1) | Golden JSON for every new type; copied into aq-server |
| `crates/aq-worker/src/bin/aq-worker/main.rs` (moved from `src/main.rs`) | move + modify (W0, W9) | CLI; `run --service-config`; `service …` subcommands |
| `crates/aq-worker/src/bin/aq-worker/{native_diagnostics.rs,native_fixture.rs,native_fixture/replay.rs,fixture_responses_cli.rs}` | move (W0) | Bin-only modules, now outside the lib count |
| `crates/aq-worker/src/bin/aq-worker/service.rs` | create (W9) | `NativeSlotBackend`, the aq-worker lib glue for `aq-node-service` |
| `crates/aq-worker/src/bin/aq-worker/measure.rs` | create (W10) | Docker-local measurement run (three Claude calls) |
| `crates/aq-worker/src/native/runner.rs` | modify (W2) | `run_until_attempt`, `poll_until_offer` |
| `crates/aq-worker/src/native/runner/recovery.rs` | modify (W2, W4) | `retained_attempts`, `salvage_interrupted` |
| `crates/aq-worker/src/sandbox_driver.rs` | modify (W3) | Handoff marker, copy-back, handoff note claim |
| `crates/aq-worker/src/native/runner/claude_docker.rs` | modify (W5) | Allow an Access-configured enrollment |
| `crates/aq-terminal-bridge/src/http.rs` | modify (W5) | `RelayHttp::with_access` |
| `crates/aq-node-service/**` | create (W6–W8, W10) | Service config, layout, server link, aq-git child, slot supervisor, OS service layer, measurement schedule |
| `packaging/service/aq-install.sh`, `packaging/service/tests/test_install.py`, `packaging/measure/{minimal,mixed,adversarial}.txt` | create (W11, W10) | Install script; measurement prompts |
| `docs/operations/install-service.md` | create (W11) | Operator guide |

### aq-server (`/home/jkern/dev/aq-platform/aq-server`)

| Path | Status | Responsibility |
| --- | --- | --- |
| `crates/aq-row-retention/**`, `crates/aq-environment-templates/**` | create (S0) | Code moved out of the aq-server lib to pay for this phase's lines |
| `vendor/aq-protocol-0.1.1/**` | create (S1) | Vendored machine wire types |
| `crates/aq-server/migrations/0100_machine_control.sql` | create (S2) | `nodes.machine_id/slot`, `machine_controls`, `attempt_handoffs`, `context_qualification_renewals` |
| `crates/aq-machine-control/**` | create (S2) | SQL store and rules for machines, controls and handoffs (no `AppState`) |
| `crates/aq-server/src/machine_api.rs` | create (S3, S4, S8) | `/v1/worker/slots`, `/v1/worker/control`, `/v1/worker/handoffs`, `/v1/worker/qualifications` |
| `crates/aq-server/src/machine_actions.rs` | create (S5) | Dashboard action kinds `worker.start` / `worker.stop` / `worker.drain` |
| `crates/aq-server/src/profiled_execution/handoff.rs` | create (S6) | Successor base from the handoff commit; handoff note comment |
| `crates/aq-context-renewal/**` | create (S8) | Pure measurement validation and fact renewal |
| `crates/aq-server/src/profiled_execution/context_renewal.rs` | create (S8) | `current_facts`, renewal storage |
| `crates/aq-server/src/intelligence/keeper.rs` | create (S9) | Sticky owner availability (owner decision; see S9) |
| `scripts/ms1-profiled.py` | modify (L0) | Acceptance overrides |

### aq-dashboard (`/home/jkern/dev/aq-platform/aq-dashboard`)

| Path | Status | Responsibility |
| --- | --- | --- |
| `contracts/pinned/dashboard-actions/v1/**`, `contracts/pinned/source.json`, `contracts/live/README.md` | modify (D1) | Re-pin |
| `src/api/adapters/workers.ts` (+ `.test.ts`) | create (D2) | `/api/workers` adapter and machine grouping |
| `src/features/settings/MachineControls.tsx` (+ `.test.tsx`), `workerActions.ts` | create (D3) | Start / Stop / Drain |
| `src/features/settings/AddMachine.tsx` (+ `.test.tsx`) | create (D4) | Enrollment code and install command |
| `src/features/settings/FrameworkStatus.tsx` | modify (D2–D4) | Uses the above |

---

## Repo and merge order

Each repo merges through one PR per row. Rows merge strictly in this order, and tasks inside a PR run in the listed order. Tasks inside one PR that touch the same subsystem are sequential, never parallel.

| # | PR | Tasks | Merges after |
| --- | --- | --- | --- |
| 1 | aq-worker `swarm/p1-protocol` | W1 | — |
| 2 | aq-server `swarm/p1-server` | S0 → S1 → S2 → S3 → S4 → S5 → S6 → S7 → S8 → S9 → S10 | PR 1 merged (S1 vendors its merge commit) |
| 3 | aq-dashboard `swarm/p1-dashboard` | D1 → D2 → D3 → D4 | Opened beside PR 2. **Merged in the same session as PR 2**: merge PR 2, re-pin D1 to PR 2's merge commit, then merge PR 3 |
| 4 | aq-worker `swarm/p1-worker` | W0 → W2 → W3 → W4 → W5 → W6 → W7 → W8 → W9 → W10 → W11 → W12 | PR 2 merged (the service calls the new server routes) |
| 5 | aq-server `swarm/p1-acceptance` | L0, then L (live) | PRs 2–4 merged |

Parallelism allowed: PR 3's D2–D4 may be developed while PR 2 is in progress, against S5's planned contract. PR 4's W0 and W2–W5 may be developed while PR 2 is in review. Nothing else runs in parallel.

---

## Task list

### W1: aq-protocol 0.1.1 machine wire types (aq-worker, PR 1)

**Files:**
- Modify: `crates/aq-protocol/Cargo.toml` (version `0.1.1`; add `serde_json.workspace = true` to `[dependencies]`)
- Modify: `Cargo.toml` (workspace `aq-protocol = { path = "crates/aq-protocol", version = "=0.1.1" }`)
- Modify: `crates/aq-protocol/src/lib.rs`
- Create: `crates/aq-protocol/fixtures/machine-v1.json`

**Interfaces — Produces** (all `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`, all structs `#[serde(deny_unknown_fields)]`, except where noted):

```rust
pub const MAX_MACHINE_SLOTS: u32 = 8;
pub const MACHINE_SCHEMA_VERSION: u32 = 1;

/// POST /v1/worker/slots, under the bearer of the machine's slot-1 node.
pub struct SlotsRequest { pub schema_version: u32, pub slots: u32 }          // slots: total wanted, 1..=8
pub struct SlotsResponse { pub schema_version: u32, pub machine_id: Uuid, pub slots: Vec<SlotEnrollment> }
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
    pub machine_id: Uuid,
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
  - `machine_fixture_round_trips`: `include_str!("../fixtures/machine-v1.json")` is a JSON object with keys `slots_request`, `slots_response`, `control_request`, `control_response`, `handoff_notice`, `measurement_submission`, `qualification_pin`. Each value deserializes into its type and re-serializes to an equal `serde_json::Value`.
  - `slot_enrollment_debug_redacts_credential`: `format!("{:?}", SlotEnrollment{slot:2,node_id:Uuid::nil(),credential:Some("aqn_secret".into())})` contains `"<redacted>"` and does not contain `aqn_secret`.
  - `desired_state_is_snake_case`: `serde_json::to_string(&DesiredState::Draining).unwrap() == "\"draining\""`; `HandoffReason::ServiceStop` serializes to `"service_stop"`.
  - `unknown_fields_are_refused`: `serde_json::from_str::<ControlRequest>(r#"{"schema_version":1,"x":1}"#)` is `Err`.
- [ ] **Step 2: Write the fixture** `crates/aq-protocol/fixtures/machine-v1.json`: one example per key above. Use `control_response.desired = "stopped"`, `generation = 2`, one handoff UUID, and a `qualification` with digest `"a"` repeated 64 times.
- [ ] **Step 3: Run and see it fail.** Run `cd /home/jkern/dev/aq-platform/wt/swarm-p1-protocol && CARGO_TARGET_DIR=/home/jkern/dev/aq-platform/target-swarm-p1-protocol timeout 900 cargo test -p aq-protocol --locked`. Expected: compile errors for the missing types.
- [ ] **Step 4: Implement the types** exactly as listed.
- [ ] **Step 5: Run it again.** Same command, then `cargo test --workspace --locked --all-features --no-run` under `timeout 900`, so that every crate still builds against 0.1.1. Expected: PASS. `python3 scripts/build-budget.py` prints OK.
- [ ] **Step 6: Commit and open PR 1.** Commit message `feat(protocol): aq-protocol 0.1.1 machine wire types (spec 0007 phase 1)`.

---

### S0: Make room in the aq-server lib (aq-server, PR 2)

Move two self-contained modules into new crates. No behaviour changes.

**Files:**
- Create: `crates/aq-row-retention/{Cargo.toml,src/lib.rs}`. Its deps are `anyhow`, `async-trait`, `sqlx`, `sqlx-postgres` and `uuid`, all `workspace = true`, as in `crates/aq-task-comments/Cargo.toml`.
- Modify: `crates/aq-server/src/row_retention.rs`. It becomes a shim with:
  - the module doc;
  - `pub use aq_row_retention::*;`
  - the unchanged `pub async fn run(state: crate::AppState, stop: tokio::sync::watch::Receiver<bool>)`.
- Create: `crates/aq-environment-templates/{Cargo.toml,src/lib.rs}`. Its deps are `aq-admission` (as aq-server declares it), `aq-profiles`, `serde`, `sqlx`, `sqlx-postgres` and `thiserror`.
- Modify: `crates/aq-server/src/templates.rs`. It becomes a shim with:
  - `pub use aq_environment_templates::{Allowlist, Resolution, resolve, catalog_environments, deserialize};`
  - two wrappers: `pub async fn store(tx: &mut sqlx::PgConnection, catalog: &Digest, templates: &[Resolution]) -> Result<(), AppError>` and `pub async fn load(conn: &mut sqlx::PgConnection, catalog_digest: &Digest) -> Result<Vec<Resolution>, AppError>`. Each maps `aq_environment_templates::Error` to `AppError::Internal`.
- Modify: root `Cargo.toml` `members` (add both crates); `crates/aq-server/Cargo.toml` (add both as path deps).

**Interfaces — Produces:**
- `aq_row_retention::{BATCH, INTERVAL, Report, TaskState, collect}`, with the same signatures as today.
- `aq_row_retention` also gains a blanket `impl<T: TaskState + ?Sized> TaskState for Option<std::sync::Arc<T>>`, which returns `Ok(false)` for `None`. It replaces aq-server's `impl TaskState for Option<Arc<crate::agent_access::Runtime>>` (row_retention.rs:18-26), which the orphan rule would forbid after the move. `agent_access/graph.rs:493` keeps `impl crate::row_retention::TaskState for Runtime`.
- `aq_environment_templates::Error` (`thiserror`, one variant `Database`).

The tests module at `templates.rs:203` moves into the new crate unchanged, and every `#[cfg(test)]` in `row_retention.rs` moves too.

- [ ] **Step 1: Write the failing test.** Add a test to `crates/aq-row-retention/src/lib.rs`: `option_arc_of_task_state_delegates_and_none_is_incomplete`. A struct `Always(bool)` implements `TaskState`. `Some(Arc::new(Always(true))).completed(..)` returns `Ok(true)`, and `None::<Arc<Always>>` returns `Ok(false)`.
- [ ] **Step 2: Run it.** Run `cd /home/jkern/dev/aq-platform/wt/swarm-p1-server && CARGO_TARGET_DIR=/home/jkern/dev/aq-platform/target-swarm-p1-server timeout 900 cargo test -p aq-row-retention --locked`. Expected: FAIL (the crate is missing).
- [ ] **Step 3: Move the code** (`git mv` is not possible across the split; cut and paste exactly). Keep every SQL string byte-identical.
- [ ] **Step 4: Run the tests and checks.**
  - `eval "$(scripts/test-db)"`
  - `timeout 900 cargo test -p aq-row-retention -p aq-environment-templates --locked`
  - `timeout 900 cargo nextest run -p aq-server --test it_schema -E 'test(/^row_retention::/)'`
  - `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/placement|template/)'`

  Expected: PASS. `python3 scripts/build-budget.py` reports the aq-server lib about 680 lines below 109,593. Do **not** run `--update` (S10 does).
- [ ] **Step 5: Commit.** Commit message `refactor: move row retention and environment templates into crates (room for spec 0007 phase 1)`.

---

### S1: Vendor aq-protocol 0.1.1 (aq-server)

**Files:**
- Create: `vendor/aq-protocol-0.1.1/`. Copy it from aq-worker `crates/aq-protocol` at PR 1's merge commit, including `fixtures/`. Add `vendor/aq-protocol-0.1.1/SOURCE.md` with one line: `Copied from ElectricJack/aq-worker <merge commit> crates/aq-protocol.`
- Delete: `vendor/aq-protocol-0.1.0/`.
- Modify: root `Cargo.toml`:
  - `aq-protocol = { path = "vendor/aq-protocol-0.1.1", version = "=0.1.1" }`;
  - in `exclude`, replace `vendor/aq-protocol-0.1.0` with `vendor/aq-protocol-0.1.1`.

- [ ] **Step 1: Copy, edit and check.** Run `cargo update -p aq-protocol --offline`, then `timeout 900 cargo test -p aq-protocol --locked` (PASS, including `machine_fixture_round_trips`), then `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::/)'` (PASS).
- [ ] **Step 2: Commit.** Commit message `build: vendor aq-protocol 0.1.1 (machine wire types)`.

---

### S2: Migration 0100 and the `aq-machine-control` store (aq-server)

**Files:**
- Create: `crates/aq-server/migrations/0100_machine_control.sql`
- Create: `crates/aq-machine-control/{Cargo.toml,src/lib.rs}`. Its deps are `aq-protocol`, `chrono`, `sqlx`, `sqlx-postgres`, `thiserror` and `uuid`.
- Modify: root `Cargo.toml` members; `crates/aq-server/Cargo.toml` (path dep).
- Test: `crates/aq-server/tests/it_access/machine_store.rs` (new module, listed in `tests/it_access/main.rs`).

**Migration, exactly:**

```sql
-- Spec 0007 phase 1: a machine groups the slot nodes of one service; the
-- owner's desired state per machine; handoff requests; renewed context facts.
ALTER TABLE nodes ADD COLUMN machine_id UUID REFERENCES nodes(id);
ALTER TABLE nodes ADD COLUMN slot INTEGER CHECK (slot BETWEEN 1 AND 8);
UPDATE nodes SET machine_id = id, slot = 1 WHERE machine_id IS NULL;
ALTER TABLE nodes ALTER COLUMN machine_id SET NOT NULL;
ALTER TABLE nodes ALTER COLUMN slot SET NOT NULL;
CREATE UNIQUE INDEX nodes_machine_slot ON nodes(machine_id, slot);
CREATE FUNCTION nodes_default_machine() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.machine_id IS NULL THEN NEW.machine_id := NEW.id; NEW.slot := 1; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER nodes_default_machine BEFORE INSERT ON nodes
  FOR EACH ROW EXECUTE FUNCTION nodes_default_machine();

CREATE TABLE machine_controls (
  machine_id UUID PRIMARY KEY REFERENCES nodes(id),
  desired TEXT NOT NULL CHECK (desired IN ('running','draining','stopped')),
  generation BIGINT NOT NULL CHECK (generation >= 2),
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
UPDATE meta SET value='100' WHERE key='state_format_version';
```

A machine with no `machine_controls` row is `running` at generation 1. Each owner change writes generation `previous + 1`, so a stored row is always ≥ 2.

**Interfaces — Produces** (`crates/aq-machine-control/src/lib.rs`):

```rust
pub use aq_protocol::{DesiredState, HandoffReason, MAX_MACHINE_SLOTS};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")] NotFound,
    #[error("generation conflict")] Conflict { current: Control },
    #[error("invalid: {0}")] Invalid(&'static str),
    #[error(transparent)] Db(#[from] sqlx::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct MachineNode { pub node_id: Uuid, pub machine_id: Uuid, pub slot: u32 }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct Control { pub machine_id: Uuid, pub desired: DesiredState, pub generation: u64 }
pub struct NewSlot<'a> { pub base_name: &'a str, pub worker_version: &'a str, pub protocol_version: u32 }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct MintedSlot { pub slot: u32, pub node_id: Uuid, pub created: bool }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct HandoffRow { pub attempt_id: Uuid, pub node_id: Uuid, pub reason: HandoffReason,
    pub candidate_operation_id: Option<Uuid>, pub candidate_oid: Option<String>, pub note_comment_id: Option<Uuid> }

pub fn slot_name(base: &str, slot: u32) -> String;                       // "<base>#<slot>", base truncated to 60 chars
pub async fn machine_of(conn: &mut PgConnection, node: Uuid) -> Result<MachineNode, Error>;
pub async fn machine_nodes(conn: &mut PgConnection, machine: Uuid) -> Result<Vec<MachineNode>, Error>; // ORDER BY slot
/// Caller must be slot 1. Creates the missing slots 2..=wanted; never re-issues an existing slot.
/// `hashes[i]` is the credential verifier for slot i+2; only the slots it creates use theirs.
pub async fn mint_slots(tx: &mut PgConnection, caller: MachineNode, wanted: u32, new: &NewSlot<'_>,
    hashes: &[String], now: DateTime<Utc>) -> Result<Vec<MintedSlot>, Error>;
pub async fn control(conn: &mut PgConnection, machine: Uuid) -> Result<Control, Error>;
/// Plain compare-and-swap: `expected_generation` must equal the current generation, else
/// Error::Conflict{current}. On success writes desired and generation+1 (also when desired is unchanged).
pub async fn set_desired(tx: &mut PgConnection, machine: Uuid, desired: DesiredState, expected_generation: u64,
    actor: Uuid, now: DateTime<Utc>) -> Result<Control, Error>;
/// Inserts an owner_stop row for every profiled attempt on the machine's nodes that holds its task and whose
/// hot stop kind is 'unknown' or 'running'. ON CONFLICT DO NOTHING. Returns the attempt ids now pending.
pub async fn request_handoffs(tx: &mut PgConnection, machine: Uuid, now: DateTime<Utc>) -> Result<Vec<Uuid>, Error>;
/// Attempts with any handoff row whose attempt still holds its task and whose stop kind is 'unknown' or 'running'.
pub async fn pending_handoffs(conn: &mut PgConnection, machine: Uuid) -> Result<Vec<Uuid>, Error>;
/// Node-announced handoff; the attempt must belong to `node`. ON CONFLICT (attempt_id) DO NOTHING.
pub async fn record_handoff(conn: &mut PgConnection, node: Uuid, attempt: Uuid, reason: HandoffReason,
    now: DateTime<Utc>) -> Result<(), Error>;
pub async fn handoff_of(conn: &mut PgConnection, attempt: Uuid) -> Result<Option<HandoffRow>, Error>;
pub async fn set_handoff_candidate(conn: &mut PgConnection, attempt: Uuid, operation: Uuid, oid: &str,
    note_comment: Option<Uuid>) -> Result<(), Error>;        // only fills NULL columns; equal values are a no-op
pub async fn accepts_new_work(conn: &mut PgConnection, node: Uuid) -> Result<bool, Error>; // desired == Running
```

The SQL for "running attempt" is:

```sql
SELECT a.attempt_id FROM execution_attempts a
  JOIN nodes n ON n.id = a.node_id
  JOIN profiled_authority_hot h USING (attempt_id)
WHERE n.machine_id = $1 AND a.kind = 'profiled_v1' AND a.holds_task
  AND h.state->'stop'->>'kind' IN ('unknown','running')
ORDER BY a.attempt_id LIMIT 64
```

`mint_slots` inserts each node with an explicit `machine_id = caller.machine_id`, `slot = n` and `name = slot_name(base, n)` (the trigger does not fire when `machine_id` is set). It returns `MintedSlot { created: false }` for slots that already exist. `wanted` must satisfy `1 ≤ wanted ≤ 8` and `caller.slot == 1`, else `Error::Invalid`.

- [ ] **Step 1: Write the failing tests** in `tests/it_access/machine_store.rs`. Each test uses `aq_server::test_support::TestDb::create().await` and `db.pool().await`. Seed a user and a node with direct SQL, copying the seed shapes from `tests/it_access/worker_api.rs`.
  - `legacy_node_is_its_own_machine_slot_1`: a node inserted without `machine_id` gets `machine_id = id`, `slot = 1`; `control()` returns `{desired: Running, generation: 1}`.
  - `mint_slots_creates_missing_slots_once`: `mint_slots(wanted = 3)` returns slots 2 and 3, both `created: true`. A second call returns the same `node_id`s with `created: false`. Calling it from slot 2 gives `Invalid`, and so does `wanted = 9`. Names are `"desk#2"` and `"desk#3"` for base `"desk"`.
  - `set_desired_is_compare_and_swap`: `set_desired(Stopped, 1)` → generation 2. `set_desired(Running, 1)` → `Conflict { current.generation == 2 }`.
  - `request_handoffs_selects_running_attempts_only`: seed two attempts with `holds_task` and hot `stop.kind` `running` and `stopped`. Only the running one is returned. A second call returns the same single id and inserts no duplicate.
  - `record_handoff_requires_the_owning_node`: another node's attempt gives `Invalid("attempt_not_owned")`.
  - `accepts_new_work_follows_desired`: `true` when running, `false` after `set_desired(Draining)`.
- [ ] **Step 2: Run them.** Run `eval "$(scripts/test-db)"; timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^machine_store::/)'`. Expected: FAIL (missing crate or migration).
- [ ] **Step 3: Write the migration and implement the crate.** Every query uses `sqlx::query`/`query_as` with binds, as `aq-task-comments` does.
- [ ] **Step 4: Run the tests and catalog checks.** Run the same command (PASS), plus:
  - `timeout 900 cargo nextest run -p aq-server --test it_schema -E 'test(/^(store|migration_catalog)::/)'`
  - `python3 scripts/migration_catalog.py`
  - `python3 scripts/test-migration-catalog.py`

  Expected: PASS, including `store::tests::state_format_is_the_latest_migration` (run `timeout 900 cargo test -p aq-server --lib store::tests::state_format_is_the_latest_migration`).
- [ ] **Step 5: Commit.** Commit message `feat(swarm): migration 0100 and aq-machine-control store (spec 0007 phase 1)`.

---

### S3: `/v1/worker/slots` — mint sibling slot nodes (aq-server)

**Files:**
- Create: `crates/aq-server/src/machine_api.rs` (module declared in `lib.rs` next to `worker_api`; routes merged into `worker_routes` in `worker_api.rs:22`).
- Modify: `protocol/commands/v1/routes.json`. Add `{"path": "/v1/worker/slots", "class": "node", "note": <same note string as /v1/worker/heartbeat>}`, keeping alphabetical order.
- Test: `crates/aq-server/tests/it_access/worker_api.rs` (existing module; reuse its `Harness`).

**Interfaces — Produces:**
- `pub fn machine_routes(state: AppState) -> Router<AppState>`, with body limit `16 * 1024`.
- `async fn slots(State(state): State<AppState>, headers: HeaderMap, ApiJson(req): ApiJson<SlotsRequest>) -> Result<Json<SlotsResponse>, AppError>`:
  1. `let auth = crate::worker_api::authenticate_context(&state, &headers).await?;`
  2. `req.schema_version == 1` and `1 ..= MAX_MACHINE_SLOTS` contains `req.slots`, else `AppError::BadRequest("slots must be 1-8")`.
  3. Begin a transaction, `lock_current_node(&mut tx, &auth)`, `machine_of`, then generate `req.slots - 1` credentials with `crate::tokens::new_node_credential()` and their `verifier()` hashes.
  4. Call `mint_slots(tx, caller, req.slots, NewSlot { base_name: &auth.node().name, worker_version: &auth.node().worker_version, protocol_version: auth.node().protocol_version }, &hashes, now)`, then commit.
  5. The response includes `credential: Some(..)` only for `created` slots.
  - Error mapping: `Error::Invalid(m)` → `AppError::BadRequest(m.into())`; `Conflict` → `AppError::Conflict`; `Db` → `AppError::Internal`.

- [ ] **Step 1: Write the failing tests** in `worker_api.rs`.
  - `slots_mint_siblings_once_with_credentials`: enroll via the harness (the existing enroll helper), then POST `/v1/worker/slots` `{"schema_version":1,"slots":3}` with the enrolled bearer.
    - Assert 200, 2 slots, each with a credential starting `aqn_`.
    - Heartbeat succeeds with each sibling credential.
    - A second call returns the same `node_id`s and no `credential` field.
  - `slots_refuses_sibling_caller_and_bounds`: a sibling bearer gets 400; `slots: 0` gets 400; `slots: 9` gets 400; no bearer gets 401.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::slots_/)'`. Expected: FAIL (404).
- [ ] **Step 3: Implement and mount the route, and add the routes.json entry.**
- [ ] **Step 4: Run them again** (PASS), then `timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'` (PASS: the route list check).
- [ ] **Step 5: Commit.** Commit message `feat(swarm): /v1/worker/slots mints a machine's slot nodes`.

---

### S4: `/v1/worker/control`, `/v1/worker/handoffs` and the offer gate (aq-server)

**Files:**
- Modify: `crates/aq-server/src/machine_api.rs`
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs` (`offers()`, immediately before the `redelivery::existing` call at the line that reads `// Redelivery precedes new scheduling.`)
- Modify: `protocol/commands/v1/routes.json` (two `node` entries)
- Test: `crates/aq-server/tests/it_access/worker_api.rs`; `crates/aq-server/tests/it_native/native_offers.rs` (existing module)

**Interfaces — Produces:**
- `async fn control(State, HeaderMap, ApiJson<ControlRequest>) -> Result<Json<ControlResponse>, AppError>`:
  1. Authenticate, then `machine_of`, `control` and `pending_handoffs`.
  2. `qualification` is `crate::profiled_execution::context_renewal::current_pin(&state, now_ms).await?`. Until S8, this is a stub that returns `Ok(None)`; S8 replaces it.
  3. Record a heartbeat with `state.store.record_heartbeat(node.id, &node.worker_version, node.protocol_version, now)`. The control poll counts as liveness.
- `async fn handoffs(State, HeaderMap, ApiJson<HandoffNotice>) -> Result<Json<HandoffNotice>, AppError>`: authenticate, then `record_handoff(node.id, attempt, reason)`. It echoes the request.
- **Offer gate.** In `offers()`:

  ```rust
  let mut gate = state.store.pool.acquire().await.map_err(db)?;
  if !aq_machine_control::accepts_new_work(&mut gate, auth.id()).await.map_err(|_| refused())? {
      tracing::info!(node_id = %auth.id(), "machine is not running (draining or stopped); no new offers");
      return Ok(page);
  }
  ```

  Cancellations are already in `page`, so a stopped machine still receives them.

- [ ] **Step 1: Write the failing tests.**
  - In `worker_api.rs`:
    - `control_reports_desired_state_and_pending_handoffs`: a fresh node gets `desired = "running"`, `generation = 1` and empty `handoff`. After a direct `set_desired(Stopped)` and a seeded running attempt plus `request_handoffs`, the response carries that attempt id and `"stopped"`, generation 2. `last_seen_at` advances.
    - `machine_control_generation_conflicts_and_idempotent_handoffs` (Review Focus 3): two `request_handoffs` calls leave one row. `set_desired` with a stale generation conflicts. POST `/v1/worker/handoffs` twice gives 200 twice and one row. Another node's attempt gives 400.
  - In `it_native/native_offers.rs`, `stopped_machine_gets_no_new_offer_but_keeps_cancellations`. Copy the setup of the nearest existing offer test in that module. `set_desired(Stopped)` → the poll returns zero offers. `set_desired(Running)` at the new generation → the offer appears.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::(control|machine_control)/)'` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::stopped_machine/)'`. Expected: FAIL.
- [ ] **Step 3: Implement, add the routes.json entries for `/v1/worker/control` and `/v1/worker/handoffs`, and add the stub `context_renewal::current_pin`.**
  - The stub lives in a new file `crates/aq-server/src/profiled_execution/context_renewal.rs` with `pub async fn current_pin(_: &AppState, _: u64) -> Result<Option<aq_protocol::QualificationPin>, AppError> { Ok(None) }`.
  - Declare it in `profiled_execution.rs`.
- [ ] **Step 4: Run them again** (PASS), plus `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'` (no regressions) and the `commands::` check.
- [ ] **Step 5: Commit.** Commit message `feat(swarm): machine control route, handoff notices and the offer gate`.

---

### S5: Dashboard action kinds `worker.start` / `worker.stop` / `worker.drain`, and `/api/workers` fields (aq-server)

**Files:**
- Create: `crates/aq-server/src/machine_actions.rs`
- Modify: `crates/aq-server/src/dashboard_api/actions/existing.rs`. Add `crate::machine_actions::register(&mut registry);` in `registry()` and `crate::machine_actions::schemas(&mut registry);` in `schemas()`.
- Modify: `crates/aq-server/src/dashboard_api/service.rs`. Add `capability("workers.control", a.owner, "Worker control requires an owner account")` and `capability("workers.enroll", a.owner, "Adding a machine requires an owner account")` after `capability("workers.read", true, "")`.
- Modify: `crates/aq-server/src/admin_api.rs`. `WorkerView` gains the fields below. `workers()` loads them with one extra query.
- Regenerate: `protocol/dashboard-actions/v1/schema.json` (`cargo run -p aq-server --example dashboard_actions_contract`) and `protocol/commands/v1/commands.json` (`AQ_REGENERATE_COMMANDS=1 timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`).
- Create fixtures: `protocol/dashboard-actions/v1/fixtures/request-worker-stop.json`, `request-worker-start.json`, `request-worker-drain.json`.
- Modify: `protocol/dashboard-actions/v1/README.md`. Add a "Worker control" section with the three kinds, payload, subject and dispositions.
- Test: `crates/aq-server/tests/it_dashboard/dashboard_worker_actions.rs` (new module in `it_dashboard/main.rs`).

**Interfaces — Produces:**

```rust
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerControl {
    pub operation_id: Uuid,
    pub machine_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_generation: String,
}
pub(crate) struct WorkerStart; pub(crate) struct WorkerStop; pub(crate) struct WorkerDrain;
pub(crate) fn register(r: &mut Registry);   // registers all three, .expect("worker control adapter")
pub(crate) fn schemas(r: &mut Registry);
```

Each kind has `KIND` `"worker.start"`, `"worker.stop"` or `"worker.drain"`, `GRANT = "worker.control"` (not in the agent grant vocabulary, so owner-only) and `TARGET = aq_command_registry::TargetRule::Subject`.

- **`authorize`:**
  - `existing::owner(state, user).await?`;
  - `request.subject.entity_type == "machine"`, `request.subject.id == p.machine_id.to_string()`, `request.subject.revision.is_none()` and `request.expected_versions.is_empty()`, else `ApiError::invalid()`;
  - the machine exists: `SELECT 1 FROM nodes WHERE id=$1 AND machine_id=id AND revoked_at IS NULL`, else `ApiError::for_status(StatusCode::NOT_FOUND)`;
  - returns `Ok(Scope::AllAuthorized)`.
- **`authorize_lookup`:** `existing::owner(state, user).await`. Refuse when `scope` is not `AllAuthorized`.
- **`execute`:** one transaction.
  - `set_desired(machine, Running|Stopped|Draining, parse(expected_generation), user.id, now)`.
  - For `worker.stop` only, then `request_handoffs(machine, now)`.
  - Then commit.
  - `Ok` → `Outcome::confirmed(vec![existing::entity("machine_control", p.operation_id, Some(control.generation.to_string()))], vec![existing::entity("machine", p.machine_id, Some(control.generation.to_string()))])`.
  - `Err(Conflict{current})` → `Outcome::conflict(BTreeMap::from([("machine".into(), current.generation.to_string())]))`.
  - Other errors → `existing::failure(AppError::Internal)`.
- **`WorkerView` additions** (serialized names):
  - `machine_id: Uuid`, `slot: u32`;
  - `desired: &'static str` (`"running" | "draining" | "stopped"`), `control_generation: u64`;
  - `running_attempts: u32`, `pending_handoffs: u32`.

  Fill `running_attempts` and `pending_handoffs` with one grouped query per `workers()` call over the "running attempt" predicate from S2 and `attempt_handoffs`.

Request fixture (`request-worker-stop.json`):

```json
{"operation_id":"5b0c2a9e-1f1e-4c55-9d7e-000000000001","kind":"worker.stop",
 "subject":{"type":"machine","id":"5b0c2a9e-1f1e-4c55-9d7e-0000000000aa"},"expected_versions":{},
 "reason":"Stop the desktop for gaming",
 "payload":{"operation_id":"5b0c2a9e-1f1e-4c55-9d7e-000000000001","machine_id":"5b0c2a9e-1f1e-4c55-9d7e-0000000000aa","expected_generation":"1"}}
```

`reason` is optional for all three kinds.

- [ ] **Step 1: Write the failing tests** in `dashboard_worker_actions.rs`, using the harness pattern of `it_dashboard/dashboard_actions.rs`.
  - `owner_stop_sets_stopped_and_requests_handoffs`: seed a machine with a running attempt and POST the fixture body. Expect 200 `confirmed`, `machine_controls.desired = 'stopped'`, generation 2, and one `attempt_handoffs` row with reason `owner_stop`.
  - `drain_and_start_change_state_without_handoffs`.
  - `stale_generation_is_a_conflict` (Review Focus 3): stop at "1", then start at "1" → `conflict` with `current_versions.machine == "2"`.
  - `non_owner_is_refused`.
  - `workers_list_reports_machine_fields`: GET `/api/workers` includes `machine_id`, `slot`, `desired`, `control_generation`, `running_attempts` and `pending_handoffs`.
  - `bootstrap_lists_worker_capabilities`: bootstrap includes `workers.control` and `workers.enroll`, both enabled for the owner.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_worker_actions::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement, regenerate both protocol files and write the fixtures and README section.** Then run `python3 protocol/dashboard-actions/v1/check.py` (PASS) and `cargo run -p aq-server --example dashboard_actions_contract -- --check` (no drift).
- [ ] **Step 4: Run the tests again** (PASS), plus `timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'` (PASS, including `every_action_kind_is_a_command_with_a_declared_grant`) and `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_actions::/)'` (no regressions).
- [ ] **Step 5: Commit.** Commit message `feat(swarm): worker start/stop/drain owner actions and machine fields on /api/workers`.

---

### S6: Successor base from the handoff commit, and the handoff note (aq-server) — **stronger model**

A replacement for an attempt with an `attempt_handoffs` row must start from the predecessor's saved work, not from its original base. The handoff note goes on the task thread.

**Files:**
- Create: `crates/aq-server/src/profiled_execution/handoff.rs` (declared in `profiled_execution.rs`)
- Modify: `crates/aq-server/src/profiled_execution/source.rs`. Add a variant to the private selection of `observe_source` (see below).
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs`. Change the `let (token, fact) = if let Some(replacement) = &replacement { … }` block (≈ lines 494-530) and the second observation (≈ lines 562-570).
- Test: `crates/aq-server/tests/it_profiled/replacement_handoff.rs` (new module in `it_profiled/main.rs`; base it on `replacement_source.rs`'s `setup()`).

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
/// None when no handoff row exists, when `state.delivery()` is None (log warn once), or when the
/// candidate is not Ready (log warn with the conflict step); the caller then keeps the old behaviour.
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

The second observation keeps `source::observe_retained(…, token.request())` for both replacement kinds. For the handoff path, `token.request().base` is the handoff commit, and `operation_id` is the new export, which is its retained byte owner.

**`observe_handoff`:**
- It calls `observe_source` with a new internal parameter `Fixed { object: base.oid, retention_handle: base.retention_handle }`.
- `selection` becomes `SourceSelection::Immutable { object: ObjectId { format: Sha1, hex: base.oid } }` and `retention_handle = base.retention_handle`.
- `evidence.insert("handoff", json!({"predecessor": base.predecessor, "candidate_operation_id": base.retention_handle}))`.
- Model this on the existing D1 `publication_head` branch (`source.rs` ≈ 150-165), which already hydrates a candidate commit from its central retention handle.

**Guardrails (must hold):**
- The 0053/0095 shared-export guard is untouched. The handoff path never reuses the predecessor's export operation.
- `assess()` and the B4 budgets are unchanged. A handoff counts as an attempt (see owner questions).
- `create_candidate` is idempotent by operation id, so a retried poll replays the same candidate. Check `delivery.frozen_inputs(op)` first, as `publication.rs:344-350` does.
- The task comment is posted at most once: its operation id is deterministic, and `note_comment_id` is set.

- [ ] **Step 1: Write the failing tests** in `replacement_handoff.rs`.
  - `handoff_successor_starts_from_saved_commit`. Use the synthetic Git owner of `replacement_source.rs`:
    - a predecessor with a `Failed` report whose retained source adds `saved.txt`;
    - an `attempt_handoffs(owner_stop)` row;
    - a poll then offers a successor whose `SourceAdmission.resolved_object.hex` equals the candidate oid (not the predecessor's base);
    - `attempt_handoffs.candidate_oid` is set;
    - one task comment with `author.kind = "system"` contains `"Handoff from attempt"`.
  - `plain_failure_successor_keeps_predecessor_export`: the same, but with no handoff row. The successor shares the predecessor's export operation. This is the regression guard for `successor_reuses_predecessor_export_only_with_exact_release_receipt`.
  - `handoff_poll_retry_replays_candidate_and_single_comment`: two polls produce one candidate operation and one comment.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^replacement_handoff::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again** (PASS), plus `timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^(replacement_|profiled_results::)/)'` (no regressions).
- [ ] **Step 5: Commit.** Commit message `feat(swarm): a handed-off attempt's successor starts from its saved commit, with the handoff note on the task`.

---

### S7: Per-boot registration receipt cap (aq-server)

Each slot attempt registers a new boot, so the lifetime cap of 1024 receipts per node (`registration.rs:197-208`) would end a slot after 1024 attempts. Make the cap per boot.

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/registration.rs:197-208`. The query becomes `SELECT count(*) FROM native_registration_receipts WHERE node_id=$1 AND response->'identity'->>'boot_id'=$2`, with `.bind(request.identity.boot_id.to_string())`.
- Test: `crates/aq-server/tests/it_native/native_registration.rs` (existing module)

- [ ] **Step 1: Write the failing test** `registration_receipt_cap_is_per_boot`. Insert 1024 receipt rows for boot A with direct SQL (the table is append-only; insert only). A Register for boot B succeeds. A 1025th distinct-operation Register for boot A is refused with `"native registration receipt capacity exhausted"`.
- [ ] **Step 2: Run it.** Run `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_registration::registration_receipt_cap_is_per_boot$/)'`. Expected: FAIL on the boot-B register.
- [ ] **Step 3: Implement the one-line query change.**
- [ ] **Step 4: Run it again** (PASS), plus the whole `native_registration::` module.
- [ ] **Step 5: Commit.** Commit message `fix(native): cap registration receipts per boot, not per node lifetime`.

---

### S8: Qualification renewal route and merged facts (aq-server) — **stronger model**

**Files:**
- Create: `crates/aq-context-renewal/{Cargo.toml,src/lib.rs}`. Its deps are `aq-protocol`, `serde_json`, `sha2`, `hex` and `thiserror`. It is pure, with no DB.
- Modify: `crates/aq-server/src/profiled_execution/context_renewal.rs` (replace the S4 stub)
- Modify: `crates/aq-server/src/machine_api.rs` (route `/v1/worker/qualifications`)
- Modify: `producer/workflow.rs:484-490` and `producer/build.rs:255-270`. Use `context_renewal::current_facts(state, now).await?` instead of `config.context_qualifications`. Replace `.map_err(|_| refused())` with a `warn` log of the `context_budget::Error` display text, then `refused()`.
- Modify: `protocol/commands/v1/routes.json` (one `node` entry)
- Test: `crates/aq-context-renewal/src/lib.rs` unit tests; `crates/aq-server/tests/it_native/native_qualification_renewal.rs` (new module in `it_native/main.rs`)

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
3. `result.modelUsage` has exactly one key, equal to `base.subject.model`. Otherwise `Model(key)`.
4. `modelUsage[model].contextWindow == base.capacity_tokens`. Otherwise `Window`.
5. `usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens ≤ prompt_utf8_bytes × base.maximum_tokens_per_utf8_byte + base.fixed_overhead_tokens`. Otherwise `Bound(call)`.

**`record`:**
1. Find each configured fact whose credential allows the node:
   - the credential is `[intelligence].credentials[*]` with `credential.profile == fact.subject.credential_profile`;
   - its `allowed_node_ids` contains `node.id()` or the node's `machine_id`.
2. Take the counting body from `config.profiled_execution.definition_bodies`, where `artifact() == fact.counting_evidence`, parsed with `serde_json::from_str(body())`.
3. `renew` with `effective_at_ms = now + EFFECTIVE_DELAY_MS`.
4. Deserialize into `Qualification`, `validate()`, take `artifact()`, and insert into `context_qualification_renewals` (`ON CONFLICT (renderer_digest) DO NOTHING`).
5. Return the pin of the first renewed fact.

No eligible fact → `AppError::Conflict("no configured qualification this node may renew")`. A `RenewalError` → `AppError::BadRequest(e.to_string())`.

- [ ] **Step 1: Write the failing tests.**
  - Unit tests in `aq-context-renewal`, with fixtures built in-test from the shapes of `~/.local/share/aq-ms1/measure/*.jsonl`:
    - `valid_submission_renews_with_new_window`;
    - `missing_or_duplicate_call_is_call_set`;
    - `changed_prompt_is_prompt_error`;
    - `other_model_or_window_refused`;
    - `over_bound_refused`;
    - `error_result_is_incomplete`.
  - In `it_native/native_qualification_renewal.rs`:
    - `renewal_extends_offers_past_config_expiry`: a config fact that expires at T. Before T, a valid submission is recorded. At T + 1 ms the poll offer's `startup.renderer_qualification` equals the renewed artifact; without the renewal, the same poll is refused.
    - `renewal_not_effective_before_delay`: right after `record`, offers still use the config fact.
    - `expired_fact_logs_warn_reason` (Review Focus 5): capture tracing at warn with the subscriber helper already used in it_native tests; the poll with no valid fact logs `"qualified context facts are unavailable, expired or ambiguous"`.
    - `node_outside_credential_cannot_renew`.
    - `control_reports_qualification_pin`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-context-renewal --locked` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_qualification_renewal::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again** (PASS), plus `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'` and `… --test it_profiled -E 'test(/^profiled_results::/)'`.
- [ ] **Step 5: Commit.** Commit message `feat(swarm): node-submitted context qualification renewals, merged into offer budgeting`.

---

### S9: Sticky owner availability (aq-server) — **owner decision required before execution**

**Why.** Selection counts an availability observation only while `now - received_at_ms ≤ observation_max_age_ms` (`aq-selection/src/candidate_core.rs:108-118`; aq-ms1 uses 30 s). Only the owner posts positive observations (`intelligence.rs:115`). An always-on swarm therefore stops getting offers about 30 s after the owner's last post.

**Default chosen here.** Server-side keeper: an owner `available` attestation stays in force until a negative observation arrives. If the owner declines, skip S9. The live acceptance then posts `ms1-profiled.py observe` by hand before each step.

**Files:**
- Create: `crates/aq-server/src/intelligence/keeper.rs`
- Modify: `crates/aq-server/src/lib.rs`. Spawn `intelligence::keeper::run(state.clone(), rx.clone())` next to `row_retention::run` (lib.rs ≈ 489).
- Test: `crates/aq-server/tests/it_access/intelligence.rs` (existing module)

**Interfaces — Produces:** `pub async fn run(state: AppState, stop: watch::Receiver<bool>)` and `pub async fn refresh_once(state: &AppState, now_ms: u64) -> Result<u32, AppError>`.
- The loop runs every `KEEPER_INTERVAL = Duration::from_secs(10)`.
- `refresh_once` takes advisory lock `712009`, the same one `observe()` takes, then:
  1. Select rows of `intelligence_observations` whose `observation->'source'->>'kind' = 'operator'`, whose `observation->>'state' = 'available'` and with `received_at_ms < now - REFRESH_AFTER_MS`, where `REFRESH_AFTER_MS = 15_000`.
  2. For each, skip it when any unexpired row with the same `scope` (ignoring `node_id`) has a source kind other than `operator` and a state other than `available`.
  3. Otherwise rewrite it with:
     - `sequence = old + 1` and a new `observation_id`;
     - `received_at_ms = now` and `expires_at_ms = now + (old.expires_at_ms - old.received_at_ms)`;
     - `cooldown_until_ms = now`;
     - `revision = registry.revision + 1`.

     Write it with the same `INSERT … ON CONFLICT` and `UPDATE intelligence_registry` statements as `observe()` (`intelligence.rs:216-224`).
  4. Return the number refreshed.

- [ ] **Step 1: Write the failing tests.**
  - `keeper_refreshes_owner_available_observation`: an owner observation 20 s old gets a new `received_at_ms == now` and a sequence one higher.
  - `keeper_never_overrides_negative_feedback`: a worker `exhausted` observation for the same scope → the owner row is left as is.
  - `keeper_ignores_owner_unavailable`.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^intelligence::keeper_/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(intelligence): keep an owner availability attestation in force until negative feedback (spec 0007 phase 1, owner decision)`.

---

### S10: Close the aq-server PR (aq-server)

- [ ] **Step 1: Run the full checks.**
  - `timeout 2400 cargo nextest run --workspace --locked --no-fail-fast` (PASS)
  - `timeout 900 cargo test --workspace --locked --doc` (PASS)
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --check`
- [ ] **Step 2: Lower the budget.** Run `python3 scripts/build-budget.py --update`. Expected: the aq-server ratchet goes **down** (below 109,593) and the test binary count stays 81. If the count went up instead, move more code out before continuing. Never raise the ratchet.
- [ ] **Step 3: Write the docs.**
  - Add `docs/implementation/0007-P1-server.md`: what was built, routes, tables, tests and limits. Copy the owner questions from this plan that are still open.
  - Add one row to `docs/specs/0007-swarm.md` §10's table note: "Phase 1 server: see `docs/implementation/0007-P1-server.md`".
- [ ] **Step 4: Commit and open PR 2.** Commit message `docs(swarm): phase 1 server notes; lower the lib ratchet`. The PR body lists migration 0100's tables, the new routes and the action kinds.

---

### D1: Re-pin the dashboard-actions contract (aq-dashboard, PR 3)

**Files:**
- Modify: `contracts/pinned/dashboard-actions/v1/{README.md,schema.json,check.py}` and `fixtures/*`. Copy them byte for byte from aq-server `protocol/dashboard-actions/v1/` at the PR 2 head commit, and add the three `request-worker-*.json` fixtures.
- Modify: `contracts/pinned/source.json`. Set `commit` to that aq-server commit, set `contract_commits["dashboard-actions"]` to the same, and recompute every changed `sha256` (`sha256sum`).
  - While PR 2 is unmerged, add `"note": "pinned to aq-server branch swarm/p1-server; re-pin to the merge commit before merging"`.
  - After PR 2 merges, repeat this task against the merge commit and remove the note.
- Modify: `contracts/live/README.md`. Under the `/api/workers` row, document the six new fields from S5.

- [ ] **Step 1: Check the copies.** Run `python3 contracts/pinned/dashboard-actions/v1/check.py` (PASS).
- [ ] **Step 2: Run the tests.** Run `pnpm test` (PASS: existing fixture-driven tests still pass) and `pnpm typecheck`.
- [ ] **Step 3: Commit.** Commit message `contracts: pin dashboard-actions with worker start/stop/drain from aq-server swarm/p1-server`.

### D2: Workers adapter and machine grouping (aq-dashboard)

**Files:**
- Create: `src/api/adapters/workers.ts`, `src/api/adapters/workers.test.ts`
- Modify: `src/features/settings/FrameworkStatus.tsx` (use `adaptWorkers`; remove the inline `workers()` validator at lines 11-23)
- Modify: `src/test/g4.ts:11` (mock rows gain the new fields)

**Interfaces — Produces:**

```ts
export type DesiredState = "running" | "draining" | "stopped";
export type WorkerNode = { id: string; name: string; worker_version: string; status: string; last_seen_at: string | null;
  machine_id: string; slot: number; desired: DesiredState; control_generation: number; running_attempts: number; pending_handoffs: number };
export type Machine = { id: string; name: string; desired: DesiredState; generation: number; nodes: WorkerNode[];
  runningAttempts: number; pendingHandoffs: number; online: number; drained: boolean };
export function adaptWorkers(raw: unknown): WorkerNode[];   // throws ContractError on any invalid row
export function groupMachines(nodes: WorkerNode[]): Machine[]; // by machine_id; name/desired/generation from slot 1; sorted by name
```

- A row missing the six new fields is adapted as `machine_id = id`, `slot = 1`, `desired = "running"`, `control_generation = 1` and counts of 0. This keeps older servers readable.
- A present but invalid field throws.
- `drained = desired === "draining" && runningAttempts === 0`.

- [ ] **Step 1: Write the failing tests** in `workers.test.ts`:
  - `groups slots under their machine`: three rows, two on one machine, give two machines; the first holds two nodes and `online` counts status `online`.
  - `treats missing machine fields as a single-slot running machine`;
  - `rejects an invalid desired state`;
  - `drained only when draining with no running attempts`.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t workers`. Expected: FAIL.
- [ ] **Step 3: Implement, and switch `FrameworkStatus` to it.** It still renders one `<li>` per node, now under a machine heading.
- [ ] **Step 4: Run the tests again** (PASS, including `FrameworkStatus.test.tsx`), plus `pnpm typecheck`.
- [ ] **Step 5: Commit.** Commit message `feat(settings): machine-grouped worker adapter`.

### D3: Start / Stop / Drain buttons (aq-dashboard)

**Files:**
- Create: `src/features/settings/workerActions.ts`, `src/features/settings/MachineControls.tsx`, `src/features/settings/MachineControls.test.tsx`
- Modify: `src/features/settings/FrameworkStatus.tsx` (render `<MachineControls machine={m} />` in each machine heading)
- Modify: `src/mocks/` (add a POST `/api/v1/actions` handler for `worker.*` kinds, following `src/mocks/playbooks.ts:194`)

**Interfaces — Produces:**

```ts
export type WorkerActionKind = "worker.start" | "worker.stop" | "worker.drain";
export const workerActionLabels: Record<WorkerActionKind, string> =
  { "worker.start": "Start", "worker.stop": "Stop now (save and hand off)", "worker.drain": "Drain" };
export function freezeWorkerAction(kind: WorkerActionKind, machine: Pick<Machine,"id"|"generation">, reason?: string): FrozenAction;
// body: {operation_id, kind, subject:{type:"machine",id}, expected_versions:{}, reason?, payload:{operation_id, machine_id, expected_generation: String(generation)}}
// FrozenAction.project is undefined; scope is all_authorized (adaptActionOperation already accepts it).
export function MachineControls(props: { machine: Machine }): JSX.Element;
```

**Behaviour.** Follow `PlaybookAction.tsx:111-203` exactly for gating, the dialog, the confirmation checkbox, the error mapping and the unsettled re-check:
- Capability `workers.control`; when it is disabled, show its reason.
- Which buttons appear:
  - **Start** only when `desired !== "running"`.
  - **Stop now** and **Drain** only when `desired === "running"`. **Stop now** also appears when `desired === "draining"`.
- The confirmation dialog text for Stop:
  - "Each running agent on {name} is stopped, its work is saved as a commit with a handoff note, and a fresh agent continues it elsewhere. This machine takes no new work until you start it." The values are spec §7.2.
  - Below that, "{runningAttempts} running agent(s) will be handed off."
- Drain dialog: "Running agents on {name} finish; no new work starts here."
- On confirm, invalidate the `worker-nodes` and `overview-worker-heartbeats` queries, as in `RunActions.tsx:9-12`.
- The status line shows the server's state:
  - Running;
  - "Draining (N running)" or "Drained";
  - "Stopped" or "Stopped, handing off N".

- [ ] **Step 1: Write the failing tests** in `MachineControls.test.tsx` (MSW `setupServer`):
  - `stop posts worker.stop with the machine generation`: assert the request body matches the S5 fixture shape and carries the `X-AQ-Request: 1` header;
  - `shows Start only when stopped`;
  - `disabled with reason without workers.control`;
  - `conflict shows refresh guidance`;
  - `confirmation is required`;
  - `status line shows handing-off count`.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t MachineControls`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again** (PASS), then `pnpm typecheck`, `pnpm lint` and `pnpm build` (the mock-leak check passes).
- [ ] **Step 5: Commit.** Commit message `feat(settings): start, stop and drain a machine (spec 0007 §7.2)`.

### D4: Add machine (aq-dashboard)

**Files:**
- Create: `src/features/settings/AddMachine.tsx`, `src/features/settings/AddMachine.test.tsx`
- Modify: `src/features/settings/FrameworkStatus.tsx` (render `<AddMachine />` below the machine list)

**Behaviour.**
- Capability `workers.enroll`.
- The "Create enrollment code" button POSTs `/api/enrollments` with `X-AQ-Request: 1` and body `{}`. The response is `{code, expires_at}`.
- The panel then shows:
  - The command, in a copyable `<code>`: `packaging/service/aq-install.sh --server <worker origin> --slots 2 --profile <profile>`, run from an aq-worker checkout. The worker origin is the bootstrap's worker origin if one exists, else the text field "Worker URL" prefilled with `window.location.origin` with the port replaced by `28081`; document this in the component.
  - The code in a separate copyable field, with the note "Paste this when the script asks for it. It works once and expires at {expires_at}." The code never goes into the command line.
- The code is held in component state only, never in storage or the URL.

- [ ] **Step 1: Write the failing tests:** `creates a code and shows it apart from the command`; `command never contains the code`; `hidden without workers.enroll`.
- [ ] **Step 2: Run them.** Run `pnpm test -- -t AddMachine`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again** (PASS), then `pnpm typecheck` and `pnpm build`. Take desktop and phone screenshots (`pnpm test:visual` or a Playwright screenshot) as `docs/design/visual-acceptance.md` asks, and attach them to the PR.
- [ ] **Step 5: Commit and open PR 3.** Commit message `feat(settings): add a machine with a single-use enrollment code`. The PR body says it must merge with aq-server PR 2 after the D1 re-pin.

---

### W0: Move the aq-worker binary out of the lib count (aq-worker, PR 4)

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

### W2: `run_until_attempt` — one registration, poll until an offer (aq-worker)

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

### W3: Driver handoff — stop, save and claim a handoff note (aq-worker)

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

### W4: Salvage an interrupted attempt after a restart (aq-worker) — **stronger model**

After a crash or reboot, the interrupted attempt must report a `Failed` result with its saved workspace, so that S6 starts the successor from it.

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

### W5: Cloudflare Access on the Claude executor's relay (aq-worker)

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

### W6: `aq-node-service` — config, layout, server link and aq-git child (aq-worker)

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
}
// Every request: Authorization: Bearer <credential>; cf-access-client-id/-secret when access is set;
// Content-Type: application/json. A non-2xx response becomes an error carrying status and the server's
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
    - `loads_valid_and_refuses_bounds`: slots 0 and 9 are refused, a relative path is refused, a bad profile is refused, and `measurement.enabled` without `token_file` or `model` is refused;
    - `defaults_fill_grace_and_renew_window`.
  - `layout::tests`:
    - `fresh_current_is_private_and_unique`: mode 0700; a second call fails while `current` exists;
    - `archive_wipes_material_and_marks_empty`;
    - `prune_deletes_only_empty_archives`.
  - `link::tests`:
    - `refuses_plain_http_off_loopback`: `http://10.0.0.5:8081` → Err; `http://127.0.0.1:28081` → Ok; `https://aq.example.com` → Ok;
    - `sends_bearer_and_access_headers`: an axum loopback server asserts the headers and the paths for the four calls;
    - `error_carries_status_not_secrets`.
  - `git::tests`:
    - `register_sends_repo_register_frames`: a fake aq-git, a tiny tokio `UnixListener` in the test answering `{"ok":true,"request_id":…,"result":{"mode":"worker"}}`; skip `start` and construct `AqGit` through a `#[cfg(test)]` constructor over an existing socket.
  - In `crates/aq-worker/tests/config.rs`:
    - `node_service_enrollment_file_is_a_worker_config`: `aq_node_service::EnrollmentFile::save`, then `aq_worker::config::WorkerConfig::load` reads the same node id and server URL.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-node-service --locked` and `timeout 900 cargo test -p aq-worker --locked --all-features --test config node_service`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(node-service): service config, state layout, server link and aq-git child`.

### W7: `aq-node-service` — the slot supervisor (aq-worker)

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
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-node-service --locked supervisor`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS. Run each test 20 times with `for i in $(seq 20); do timeout 900 cargo test -p aq-node-service --locked supervisor -q || break; done` (no flakes).
- [ ] **Step 5: Commit.** Commit message `feat(node-service): slot supervisor with handoff, renderer switch, backoff and graceful stop`.

### W8: `aq-node-service` — OS service layer, systemd user units (aq-worker)

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

### W9: CLI glue — `run --service-config` and `service …` (aq-worker)

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
    /// Enroll slot 1 with a single-use code (prompted, or the first stdin line), mint slots 2..N, write service.toml.
    /// With --cf-service-token the next two prompts/lines are the node Access client id and secret.
    Enroll { #[arg(long)] profile: String, #[arg(long)] server: String, #[arg(long, default_value_t = 1)] slots: u32,
             #[arg(long)] name: Option<String>, #[arg(long)] cf_service_token: bool,
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
1. Read the secrets with `read_secrets_from`: the code, plus the CF id and secret if `--cf-service-token`. `--runner-access-token` is not offered; see owner question 8.
2. `Client::new(&server, None, cf…).enroll(EnrollRequest{ name: name or hostname, … })` gives slot 1.
3. Save slot 1 with `EnrollmentFile::save(layout.slot_enrollment(1))`.
4. If `slots > 1`, `ServerLink::from_enrollment(slot1).slots(&SlotsRequest{schema_version:1, slots})`. Save each slot that has a credential as `EnrollmentFile` with the same `server_url` and cf values.
   - A slot without a credential (already minted earlier and lost) is an error: "slot N exists on the server but its credential is not on this machine; revoke the machine and enroll again".
5. Copy `native_template` to `config_dir/native.json` (0600).
6. Write `service.toml`.
7. Print `machine <id>: slots <ids…>`. Never print secrets.

**`run_service`:**
1. Load the config.
2. Use slot 1's `ServerLink` as the `ControlSource`.
3. Build `Supervisor` with `NativeSlotBackend`.
4. Run it with the `shutdown_signal()` watch (SIGTERM or ctrl-c).

- [ ] **Step 1: Write the failing tests** in `tests/controller_cli.rs`:
  - `run_requires_exactly_one_config_kind`: `run` alone fails; `run --controller-config x --service-config y` fails; `run --service-config /nonexistent` exits 2 with the path in stderr.
  - `service_enroll_never_accepts_secrets_in_argv`: `--code` is not a flag (parse error).
  - `service_status_without_install_reports_not_installed`: set `XDG_CONFIG_HOME` to a tempdir on the child process only (`Command::env`, never `std::env::set_var`).
  - Keep `run_reports_missing_worker_config_and_malformed_controller_config` passing.
  - Update `main.rs`'s `run_requires_explicit_controller_configuration` to accept either flag.
- [ ] **Step 2: Run them.** Run `timeout 900 cargo test -p aq-worker --locked --all-features --test controller_cli` and `--bin aq-worker`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(worker): aq-worker run --service-config and service enroll/install/status/measure`.

### W10: Measurement run and prompts (aq-worker) — **stronger model**

**Files:**
- Create: `packaging/measure/{minimal,mixed,adversarial}.txt`. Copy them from `~/.local/share/aq-ms1/measure/*.txt`. Their SHA-256 must equal the `prompt_sha256` rows in the active `aq.ms1-counting` Definition. Verify with `python3 -c` against `~/.local/share/aq-ms1/catalog-rev2.json` or the server's definition bodies.
- Create: `crates/aq-node-service/src/measure.rs`: `pub const PROMPTS: [(&str, &[u8]); 3]`, using `include_bytes!` of the three files.
- Create: `crates/aq-worker/src/bin/aq-worker/measure.rs`: `pub async fn run(template: &Path, token_file: &Path, model: &str) -> anyhow::Result<MeasurementSubmission>`.

**`measure::run` contract:**
1. Load the native template. Take `settings.host.docker_local` and `settings.host.template` (the pinned Claude image digest) and `settings.host.claude_executable`.
2. `DockerLocalBackend::connect_unix` and create one sandbox:
   - owner `{ node_id: slot-1 node, boot_id: Uuid::new_v4(), attempt_id: Uuid::new_v4() }`;
   - resources `{cpu_millis: 1000, memory_bytes: 2 GiB, pids: 256}`;
   - hard deadline now + 15 min;
   - no agent channel.
3. `put_archive` `/aq/in/{minimal,mixed,adversarial}.txt` plus a runner script. The script reads the token from `/aq/private/material`, deletes it, and for each prompt runs:

   ```sh
   CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 "$exe" --print --output-format stream-json --verbose --model <model> < /aq/in/<call>.txt > /aq/out/<call>.jsonl
   ```

   `<model>` is the `model` argument, from `service.toml` `measurement.model`. For aq-ms1 it is `claude-sonnet-5-5`, the `MODEL` in `scripts/ms1-profiled.py`. The server refuses any other model (S8 rule 3).
4. Deliver the token with `deliver_private_material` (the bytes from the 0600 token file, parsed with `sandbox_driver::parse_token`). Then `start_runner`, wait for `runner_exit` (poll every 2 s, deadline 10 min), and `get_archive` `/aq/out`.
5. Stop and destroy the sandbox, with destroy authorization `operator_released(sandbox, "aq-swarm-measurement")`.
6. Leak check: refuse if any jsonl `holds_token`.
7. Build `MeasuredCall`s from the last `{"type":"result"}` event of each file.

The flags must reproduce the existing measurements. Before submitting for the first time, run `python3 /home/jkern/dev/aq-platform/aq-server/scripts/ms1-profiled.py` `measured()` logic on the produced files: write them to a temp `measure/` dir and import the function. Every bound must pass. If the existing jsonl show different flags (inspect their `system`/`init` event), use those.

- [ ] **Step 1: Write the failing test.** `measure::tests::prompts_match_the_counting_definition` (in `aq-node-service`) asserts the three prompt SHA-256 values, hard-coded from the counting Definition.
- [ ] **Step 2: Do a live check, by hand and recorded.** Run `aq-worker service measure --profile local` (after W11 installs) and record the server's response pin in `docs/implementation/0007-P1-evidence/measure.json`.
- [ ] **Step 3: Commit.** Commit message `feat(node-service): docker-local qualification measurement and renewal submission`.

### W11: Install script and operator guide (aq-worker)

**Files:**
- Create: `packaging/service/aq-install.sh` (bash, `set -euo pipefail`)
- Create: `packaging/service/tests/test_install.py` (pytest, `unittest` style like `packaging/worker/tests/test_worker_ctl.py`; uses `shellcheck` when present and a fake `PATH` with stub `systemctl`, `loginctl` and `aq-worker`)
- Create: `docs/operations/install-service.md`

**Script interface:**

```
aq-install.sh --server URL [--profile NAME=local] [--slots N=1] [--name NAME] [--cf-service-token]
              --bundle DIR --native-template FILE --repositories FILE [--measurement-token-file FILE --measurement-model MODEL]
              [--sandbox-bundle podman-linux-amd64.tar.gz]
```

**Steps, in order (spec §7.3):**
1. **Preflight.**
   - `uname -s` is Linux, else "macOS is phase 5".
   - If `/proc/sys/kernel/osrelease` contains `microsoft` and `ps -p 1 -o comm=` is not `systemd`, print the `/etc/wsl.conf` instructions and exit 3.
   - `systemctl --user show-environment` must succeed.
   - If `loginctl show-user "$USER" -p Linger --value` is not `yes`, print `sudo loginctl enable-linger $USER` (continue).
2. **Sandbox runtime.** If `$XDG_RUNTIME_DIR/aq-sandbox/podman.sock` is missing:
   - with `--sandbox-bundle`, run `scripts/sandbox-daemon/aq-sandbox-podman.sh install <bundle>` and `systemctl --user enable --now aq-sandbox-podman.service`;
   - without it, exit 4 with that instruction.
3. **Binaries.** `install -D -m 0755` `aq-worker`, `aq-session-host`, `aq-agent`, `aq-git` and `aq-sandbox-driver` from `--bundle` into `~/.local/share/agent-q/runtime/`, and symlink `~/.local/bin/aq-worker`.
4. **Enroll.** Run `aq-worker service enroll …`, which prompts for the code itself (secrets never pass through this script).
5. **Install and start.** Run `aq-worker service install --profile P`.
6. **Measure.** If `--measurement-token-file` was given, run `aq-worker service measure --profile P`.
7. **Status.** Print `aq-worker service status --profile P` and the reminder: "Add machine <id> and slot nodes <ids> to the project binding and the credential's allowed_node_ids" (see owner question 4).

- [ ] **Step 1: Write the failing tests:**
  - `refuses_wsl_without_systemd`;
  - `never_passes_secrets_in_argv`: the stub `aq-worker` records its argv; no argv contains the stdin code;
  - `installs_binaries_and_calls_enroll_install_measure_in_order`;
  - `exits_4_without_sandbox_socket_or_bundle`.
- [ ] **Step 2: Run them.** Run `python3 -m pytest packaging/service/tests -q`. Expected: FAIL.
- [ ] **Step 3: Implement the script and write the guide.** The guide covers prerequisites (link the existing `install-worker.md` sections on WSL2 autostart and `aq-wsl-autostart.ps1`), install, the Access token, start/stop/drain semantics, restart behaviour, upgrade (`install -m 0755` the new bundle, then `systemctl --user restart aq-node@P`) and uninstall.
- [ ] **Step 4: Run them again.** Expected: PASS.
- [ ] **Step 5: Commit.** Commit message `feat(packaging): one-script service install with single-use enrollment (spec 0007 §7.3)`.

### W12: Close the aq-worker PR (aq-worker)

- [ ] **Step 1: Run the full checks.**
  - `timeout 2400 cargo test --workspace --locked --all-features` (PASS)
  - `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  - `cargo fmt --all -- --check`
  - `python3 scripts/package-admission.py --check`
- [ ] **Step 2: Lower the budget.** Run `python3 scripts/build-budget.py --update`. The aq-worker ratchet must go **down** from 71,389, and the test binaries stay 105.
- [ ] **Step 3: Write the notes.** Add `docs/implementation/0007-P1-worker.md`, in the style of `0006-P3.md`.
- [ ] **Step 4: Commit and open PR 4.** Commit message `docs(swarm): phase 1 worker notes; lower the lib ratchet`.

---

### L0: Acceptance stack preparation (aq-server, PR 5)

**Files:**
- Modify: `scripts/ms1-profiled.py`:
  - `allowed` also includes every UUID in env `AQ_MS1_EXTRA_NODE_IDS` (comma-separated, validated with `uuid.UUID`);
  - the catalog's `observation_max_age_ms` reads env `AQ_MS1_OBSERVATION_MAX_AGE_MS` (default `30000`, max `3600000`);
  - `max_lineage_elapsed_ms` reads env `AQ_MS1_LINEAGE_MS` (default `3600000`, max `604800000`).
- Modify: `scripts/ms1-demo.sh`. Add `service-status` (prints `aq-worker service status --profile ms1` and `/api/workers` machine fields) and `handoffs` (psql: `SELECT * FROM attempt_handoffs ORDER BY requested_at DESC LIMIT 5`).
- Test: `scripts/tests` has none for ms1. Instead check `python3 -c "import ast,sys; ast.parse(open('scripts/ms1-profiled.py').read())"` and run `AQ_MS1_EXTRA_NODE_IDS=bad python3 scripts/ms1-profiled.py configure`, which must exit non-zero with "invalid node id".

- [ ] **Step 1: Implement and run the checks above.**
- [ ] **Step 2: Commit.** Commit message `scripts(ms1): acceptance overrides for the swarm phase 1 service`.

### L: Live acceptance on the aq-ms1 stack — **stronger model**

Stack: `http://127.0.0.1:28080` (admin and dashboard) and `http://127.0.0.1:28081` (worker), via `~/.local/share/aq-ms1/up.sh`. The `rebuild-server`, `profiled` and `import-login` commands are documented in `scripts/ms1-demo.sh`. Evidence goes in `docs/implementation/0007-P1-evidence/` (aq-server), with secret-free files only. Before committing, run `grep -rE 'aqn_|aqa_|aqe_|sk-ant' docs/implementation/0007-P1-evidence` (expect no output).

**Preparation:**
1. Rebuild the server from aq-server main (PRs 2 and 5 merged) with `AQ_MS1_SERVER_SRC` pointing at a fresh worktree of main, then `up.sh rebuild-server`. Rebuild the dashboard bundle from aq-dashboard main.
2. Rebuild aq-worker from main with `rebuild-worker.sh --driver`. The driver binary changed (W3), so its digest changes.
3. Then rerun `up.sh profiled` with a bumped `AQ_MS1_REVISION` and `AQ_MS1_OBSERVATION_MAX_AGE_MS=3600000 AQ_MS1_LINEAGE_MS=86400000`, so that the native.json pins the new driver digest.
4. The existing E6a unit `aq-worker@local.service` points at another server (port 32839). Leave it as it is, but record `systemctl --user status aq-worker@local` in the evidence, so that its sandboxes are not confused with the new ones (each node only sees its own labels).
5. Seed two fresh tasks with `python3 scripts/ms1-task.py`. Each prompt must keep Claude busy long enough to interrupt: "create files step1.txt … step6.txt, one per minute, running `sleep 60` between them, then publish".

**Step 1: Install the service on this WSL2 desktop.**
1. Create an enrollment code in the dashboard: Settings › Framework connection › Add machine. Screenshot it, with the code redacted.
2. Run:

   ```sh
   packaging/service/aq-install.sh --server http://127.0.0.1:28081 --profile ms1 --slots 2 \
     --bundle ~/.local/share/aq-ms1/runtime \
     --native-template ~/.local/share/aq-ms1/node/config/native.json \
     --repositories <file with the ms1 repository binding> \
     --measurement-token-file ~/.config/aq-secrets/claude-oauth-token --measurement-model claude-sonnet-5-5
   ```

   The repository binding is `{"repository_id":"e0000000-0000-4000-8000-000000000003","identity":"e0-development","binding_generation":1,"object_format":"sha1"}`. Paste the code at the prompt.
3. Add the printed machine and slot node ids: `AQ_MS1_EXTRA_NODE_IDS=<ids> up.sh profiled` with a bumped revision.
4. Post the owner availability observation: `cd aq-server && python3 scripts/ms1-profiled.py observe`.

Expected:
- `systemctl --user is-active aq-node@ms1` prints `active`.
- The dashboard shows machine "<hostname>" with 2 slots online and Running.
- `aq-worker service measure` returned a pin; record it.
- Within one poll interval, both tasks start (two sandboxes; `docker -H unix://$XDG_RUNTIME_DIR/aq-sandbox/podman.sock ps`).

**Step 2a: Reboot-equivalent, graceful.** While an agent has written `step2.txt`, run `systemctl --user restart aq-node@ms1`.

Expected:
- The journal shows `HandoffNotice(service_stop)` and a driver claim "Handoff (service stop)".
- The attempt reports `Failed` and D3 records rework.
- An `attempt_handoffs` row exists with `candidate_oid` set.
- After the service comes back, a fresh attempt for the same task starts. Its workspace already contains `step1.txt` and `step2.txt`; check the successor's first commit parent, or `git show <candidate_oid> --stat` in the aq-git volume.
- The task thread has the system comment "Handoff from attempt …".

**Step 2b: Reboot-equivalent, crash.** Repeat with `systemctl --user kill -s KILL aq-node@ms1`. systemd restarts the unit.

Expected:
- On startup the supervisor logs `recover` for the slot, and salvage gives `Salvaged`.
- The successor starts from the salvaged commit, with comment reason "restart".

**Step 3: Stop from the dashboard.** With an agent running, press Stop now on the machine and confirm.

Expected:
- Within 10 s, the slot's driver logs the handoff and the attempt reports `Failed` with the handoff note.
- `/api/workers` shows `desired: "stopped"` and `pending_handoffs` falls to 0.
- No new attempt starts on this machine. The successor stays unoffered, because no other machine exists in phase 1. Record that this is expected.
- The service stays `active` and the nodes stay `online`.

**Step 4: Start again.** Press Start.

Expected:
- `desired: "running"` with a higher generation.
- The successor is offered to this machine and starts from the handoff commit.
- The task completes and D1 publishes.

**Finally:**
1. Capture `up.sh status`, `up.sh service-status` and `up.sh handoffs`, the dashboard screenshots (desktop and phone), and the server log lines filtered by `attempt_id`.
2. Write `docs/implementation/0007-P1-acceptance.md` with each step's observed outcome, IDs and timestamps (no durations claimed as estimates).
3. Commit on aq-server main with message `docs(swarm): phase 1 live acceptance on aq-ms1 [skip ci]`.

If any step fails, use superpowers:systematic-debugging. Record the defect with its owner task in the acceptance doc. Fix it in a follow-up PR on the owning repo, then rerun from the failed step.

---

## Owner questions (decide before or during execution)

1. **The handoff budget.** A handoff counts as an attempt in B4's lineage (`MAX_ATTEMPTS = 3`, 240 sandbox-minutes, 24 h) and in the selection policy's lineage limits (aq-ms1: 1 h).
   - So stopping the same task's machine three times blocks the task with `lineage_retry_budget_exhausted`.
   - A handoff more than an hour into a lineage is refused by selection. MS1.md records that this refusal currently fails the whole poll, which is a wedge risk.
   - Should owner-initiated handoffs be exempt from the attempt count? The plan does **not** exempt them.
2. **Sticky availability (S9).** May the server keep an owner "available" attestation in force until negative feedback? Without S9, an always-on swarm stops receiving offers about 30 s after the owner's last `observe`.
3. **The measurement credential.** The plan reads the owner's Claude token from a 0600 file on the measuring machine (as MS1 does today) to run the three qualification calls. The alternative is a new server-leased measurement material, which this plan does not include. Phase 4 VMs cannot use the file option.
4. **Machine authority.** Each slot node must be in the credential's `allowed_node_ids` ([intelligence], static config, needs a server restart) and in the project binding.
   - The plan leaves this as an explicit owner step per new machine.
   - The other option is for the server to treat a node as allowed when its `machine_id` is listed. This touches 6 checks (`intelligence.rs:167`, `intelligence/inventory.rs:212`, `resolver.rs:133,533`, `login_refresh/api.rs:208`, `material.rs:75`).
   - Or to auto-authorize every enrolled machine.
5. **Coordination with phase 2.** This plan defines `machine_id`/`slot` on `nodes` and the per-machine desired state. The phase-2 read model (machines vs nodes, §9) must build on these instead of introducing another machine table. The phase-2 plan was written in parallel and may disagree.
6. **The single-desktop handoff.** With no other machine (VMs are phase 4), a stopped desktop's successor waits until Start. Is that acceptable for phase 1? Spec §1's gaming scenario fully works only after phase 4.
7. **Distribution of the install bundle.** The script installs from a local bundle directory (binaries plus the pinned `native.json`). Serving the bundle and pinned definitions from the server or a release is not in this plan. Because `native.json` pins exact binary digests, every machine needs the same build.
8. **The runner Access token.** `service enroll` stores only the node Access token. The B6b runner token (`runner-access.json`) is unused by docker-local; confirm it is not needed until phase 4.
9. **Line ratchets.** The plan pays for its lines by moving code: `row_retention` and `templates` into crates, and the worker binary under `src/bin`. If the owner prefers a reviewed ratchet raise instead, S0 and W0 can be dropped.
