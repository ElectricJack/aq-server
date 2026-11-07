# Swarm phase 2 (Swarm page, read-only) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the read-only Agents › Swarm page: a server swarm read model (machines, worker nodes, running agents, live CPU/RAM including Windows host load on WSL2, free slots, offline/stopped/booting/holding states, waiting reasons) and the dashboard tree page with its properties pane and an agent terminal that opens with control already taken.

**Architecture:**
- **aq-worker** samples machine load in a new crate (`aq-machine-load`) and posts a `MachineReport` every 15 s to a new worker route. A Windows-side PowerShell probe writes the Windows host load to a file that the WSL2 worker reads.
- **aq-server** stores the reports (migration 0100), and a new crate (`aq-swarm`) assembles `GET /api/v1/swarm` from the existing capacity read, the session read, the machine reports and the measured-context facts. The existing capacity assembly moves into that crate, so the aq-server lib, which sits exactly at its line ratchet, does not grow.
- **aq-dashboard** pins the new contract and renders the tree with `@xyflow/react` 12 and `@dagrejs/dagre` at `/agents`. The roster moves to `/agents/sessions`.

**Tech Stack:** Rust 2024 (axum 0.8, sqlx 0.8 on PostgreSQL, serde, schemars 1); React 19.3 with React Router 8, TanStack Query 5, CSS Modules, `@xyflow/react` 12.11.3 and `@dagrejs/dagre` 1.1.8 (exact v1 versions), vitest 5, MSW 3, Playwright 1.64; PowerShell 5.1 for the Windows probe.

**Spec:** `aq-server/docs/specs/0007-swarm.md`, especially §5 (page), §6 (WSL2 host load) and §10 (phase 2 row). The approved mockup is `aq-dashboard/.superpowers/brainstorm/2382938-1794083621/content/swarm-tree-v3.html`.

## Global Constraints

- **Repositories and branches:** aq-server `swarm-p2/server`, aq-dashboard `swarm-p2/dashboard`, aq-worker `swarm-p2/worker`. Each branch starts from that repo's `origin/main` and becomes one PR per repo. Never use `git stash`. Never edit `/home/jkern/dev/agent-queue2`.
- **aq-server build rules (AGENTS.md):**
  - integration tests go only into the existing 8 binaries (`it_profiled`, `it_native`, `it_phase`, `it_schema`, `it_playbook`, `it_dashboard`, `it_access`, `it_server`); no new `[[test]]` target and no new `tests/*.rs` top-level file;
  - `build-budget.toml` values only go down;
  - the aq-server lib is at **109,592 of a 109,593 ratchet**, so every line added to `crates/aq-server/src` must be paid for in the same PR (Task S2 pays for this phase);
  - new code goes into new crates;
  - migrations are contiguous: the next one is `0100`. If phase 1 merges a `0100` first, renumber to the next free number and change the `UPDATE meta SET value='N'` line to match;
  - every `cargo test`/`cargo nextest` runs under `timeout 900`;
  - each worktree builds into its own `./target`, or a `CARGO_TARGET_DIR` only it uses. Never share one.
- **aq-worker build rules:**
  - `[test_binaries] max = 105` and `aq-worker = 71389` lib lines (the current count, so zero headroom);
  - new code goes in the new crate `crates/aq-machine-load`;
  - `crates/aq-worker/src/main.rs` is a bin target and is not counted;
  - new integration tests go into an existing `crates/aq-worker/tests/*.rs` file.
- **aq-dashboard rules:**
  - pnpm 10.34.5, exact dependency pins;
  - text never below 12px: map the mockup's 11px and 10px to `--font-size-xs` (12px);
  - follow `AGENTS.md`, `docs/design/visual-acceptance.md` and the compact rules (section headings 12px uppercase `letter-spacing:.06em` weight 600 `--color-text-secondary`; pane buttons 32px tall on desktop, 44px under `pointer: coarse`);
  - never render invented counts, status or load. Unknown stays "unknown", never 0.
- **Contracts:**
  - a server change to `protocol/dashboard-read/v1` merges together with the dashboard re-pin (owner rule, retrospective W76);
  - `swarm.read` is **not** added to `src/app/bundle-requirements.json`, so an older server degrades only the Swarm view;
  - the dashboard adapter tolerates unknown fields and unknown enum values on the swarm read.
- **Copy rules:** UI strings are exactly as written in this plan. Never show "phase 1/2/3" to users.
- **No time estimates** anywhere: in code comments, PR text or reports.
- **Commits** end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. PR descriptions end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.

## Decisions made in this plan (owner may revisit; listed again in the report)

1. **Controls bar is left out** in phase 2. It is not a read-only placeholder: there are no slider values to show without inventing them. The page header holds the title, tabs and legend. Phase 3 inserts the bar above the stage.
2. **Machines vs nodes.**
   - A *machine* is a group of nodes that report the same `machine.key` from the worker's `machine.json`.
   - A node with no report, or no key, is its own machine, with id `node:<node uuid>`.
   - `kind` comes from the report (`local` by default). A node with no report is `cloud` if its backend is `boat`, and `local` otherwise.
3. **Load transport.**
   - A new authenticated worker route, `POST /v1/worker/machine`, carries a `MachineReport` every 15 s.
   - `HeartbeatRequest` is not extended. Extending it would need a re-vendor of `aq-protocol` plus a worker-candidate bump of 182 commits.
   - The wire contract is `aq-server/protocol/worker-machine/v1`, and aq-worker pins a copy.
4. **Units.**
   - Load is in basis points, 0–10000, matching `NodeStorage.used_bps`. The dashboard shows `Math.round(bps / 100)` %.
   - Fresh means received within 60 s.
   - The 15-minute average is the mean of the samples received in the last 900 000 ms.
5. **Windows host load.**
   - A per-user logon scheduled task runs `aq-host-load.ps1`. Every 5 s it writes `C:\ProgramData\agent-q\host-load.json` atomically.
   - The worker reads that file, through a read-only mount in the container or `/mnt/c/...` when host-native. It uses the file when the file is at most 30 s old.
   - On WSL2 without a fresh file, the report uses Linux load with `host_probe = stale|missing`, and the page says so.
6. **Holding.**
   - A node's `holding` lists the placement-ineligibility reasons except `worker_offline`/`worker_revoked` (shown as state) and `node_concurrency_ceiling_reached` (shown as no free slots). It adds `measurement_expired`/`measurement_unavailable` for natively registered nodes, and `worker_stopped`/`worker_draining` when phase 1 reports them.
   - A machine is `holding` when it is online and any of its online nodes holds.
7. **Waiting branch.**
   - The read model carries `waiting.reasons`, swarm-wide reasons why new work cannot start now, and `waiting.agents`, which is always empty until phase 3 placement produces per-agent waits.
   - The dashboard draws the waiting branch when `waiting.agents` is non-empty, or when the overview's `ready` count is > 0 and `waiting.reasons` is non-empty.
8. **Intelligence class** has no source in v2 today. The server derives a provisional class from the model name: `haiku|mini|spark|flash` → fast, `sonnet` → standard, `opus` → deep, otherwise none (grey pill, "unclassified"). Phase 3 replaces this with the router's class.
9. **Terminal.**
   - The Swarm pane passes `autoControl` to `InteractiveTerminal`. It takes control once, after the first terminal info, and never again on automatic reconnect: spec 0002 §6's reconnect rule still holds.
   - A viewer whose lease was taken sees "Another viewer took control. You are watching."
   - The viewer who took it sees "You took control from another viewer."
   - Viewers without `terminal.control` get watch-only and no attempt.
   - The 80-column floor (0002 §6) does not fit the mockup's 360px pane. The pane widens to `min(720px, 55%)` while an agent is selected.
10. **Phase 1 seam:** see "Interfaces with phase 1" below.

## Interfaces with phase 1 (exact names)

Phase 1 must produce, and phase 2 consumes, exactly these:

- **Contract field** `SwarmNode.service: WorkerService { state: ServiceState, since: Option<Timestamp> }`.
  - `ServiceState` serializes as `"running" | "stopped" | "draining" | "unreported"`.
  - Phase 2 always emits `unreported`.
- **Server seam:**
  - `aq_swarm::service::states(conn: &mut sqlx::PgConnection, nodes: &[Uuid]) -> sqlx::Result<BTreeMap<Uuid, ServiceFact>>`, where `pub struct ServiceFact { pub state: ServiceState, pub since_ms: u64 }`.
  - Phase 2's body returns an empty map.
  - Phase 1 replaces the body to read its own service-state table. If phase 1 merged first, Task S4 writes the body against phase 1's table.
- **Hold codes phase 1 makes reachable** (already mapped by phase 2): `worker_stopped` ("Worker is stopped" / "Start the worker") and `worker_draining` ("Worker is draining" / "Wait for running agents to finish, or start the worker again").
- **Machine state `stopped`:** produced by phase 2's assembly when every online node of a machine reports `stopped`.
- **Dashboard buttons:**
  - Machine pane: `Start worker`, `Stop worker`, `Drain`.
  - Node pane: `Drain node`, `Restart`, `View logs`.
  - All render disabled while `bootstrap.capability("swarm.worker.control")` is not enabled, with the visible note "Worker start, stop and drain are not available on this server yet." Phase 1 advertises `swarm.worker.control@1` and wires the clicks to its dashboard actions.
  - Suggested action kinds, which phase 1 owns: `worker.start`, `worker.stop`, `worker.drain`, each with body `{ node_id }`.
- **Machine identity:** phase 1's install script writes `machine.json` (format in Task W1) beside the controller config, with `key` = the lower-cased host name.

## Interfaces with phase 3 (fields reserved now)

- **Reserved hold codes** (no producer in phase 2): `cpu_above_threshold`, `memory_above_threshold`, `agent_maximum_reached`.
- **`waiting.agents[]`:** each entry carries `provider`, `intelligence_class` and its own `reasons`.
- **`SwarmMachine.agent_max`:** phase 2 = the sum of the node slot totals when all are known. Phase 3 = the Settings value.

## Review Focus

1. **A WSL2 machine whose probe stopped writing.** A stale `host-load.json` must not be shown as live Windows load. The page must say "Windows host probe stale: showing WSL VM load". This is pinned by `aq-machine-load` test `stale_host_file_falls_back_to_linux_and_says_stale` (W1) and dashboard test `machine pane names a stale Windows probe` (D6).
2. **A node that never reported a slot total** (host-native, or `aq-worker run` before phase 1). The node must show `occupied/?` slots and no free-slot pills, never `0` free. This is pinned by `graph.test.ts` `unknown slot total draws no free pills` (D2) and S4 test `unknown_slot_total_gives_no_free_slot_reason`.
3. **A machine that goes offline while agents are recorded on it.** It stays in the tree greyed with its last-seen age. Its load is omitted, not shown frozen. This is pinned by S4 test `offline_machine_omits_load_and_keeps_agents` and D5 test `offline machine renders dashed and greyed without meters`.
4. **Two viewers opening the same agent.** The second takes control and the first gets a visible notice; neither keeps typing into a lost lease. This is pinned by the D4 client tests `lease taken by another viewer drops control and raises the notice` and `taking over another viewer's lease reports it`.
5. **An older server without `/api/v1/swarm`, or a 403 for a non-owner.**
   - The Agents page must still load. The Swarm tab says "Swarm is not available on this server." and links to Sessions.
   - The rest of the dashboard is untouched.
   - This is pinned by D3 test `swarm without capability degrades to a note and keeps the Sessions tab`.

---

## File structure

**aq-server**
- `crates/aq-dashboard-contract/src/swarm_contract.rs` (new): the swarm read types.
- `crates/aq-dashboard-contract/src/lib.rs`: `pub mod swarm_contract; pub use swarm_contract::*;` and the `Response::Swarm` variant.
- `crates/aq-swarm/` (new crate):
  - `Cargo.toml`
  - `src/lib.rs` (module list and `timestamp`)
  - `src/capacity.rs` (moved from aq-server)
  - `src/machine.rs` (wire report, validation, store)
  - `src/service.rs` (phase 1 seam)
  - `src/assemble.rs` (pure swarm assembly)
  - `src/class.rs` (provisional intelligence class)
  - `src/tests.rs`
- `crates/aq-server/migrations/0100_swarm_machine_reports.sql` (new).
- `crates/aq-server/src/dashboard_api/capacity.rs`: slimmed to queries and calls into `aq_swarm::capacity`.
- `crates/aq-server/src/dashboard_api/swarm.rs` (new, at most 110 lines): gathers the inputs and calls `aq_swarm::assemble::assemble`.
- `crates/aq-server/src/dashboard_api.rs`: the `/api/v1/swarm` route and handler; `mod swarm;`.
- `crates/aq-server/src/dashboard_api/service.rs`: the `swarm.read` capability.
- `crates/aq-server/src/dashboard_api/errors.rs`: `From<aq_swarm::TimeError> for Error`.
- `crates/aq-server/src/worker_api.rs`: the `/v1/worker/machine` route and handler.
- `crates/aq-server/tests/it_access/worker_api.rs`: machine report tests.
- `crates/aq-server/tests/it_dashboard/dashboard_swarm.rs` (new module) and `tests/it_dashboard/main.rs` (`mod dashboard_swarm;`).
- `protocol/worker-machine/v1/{README.md,schema.json,check.py,fixtures/report-linux.json,fixtures/report-windows-host.json}` (new).
- `protocol/dashboard-read/v1/{schema.json,README.md,check.py,fixtures/swarm.json}`.
- `scripts/ms1-demo.sh` (`cmd_node` writes `machine.json`).
- `Cargo.toml` (workspace members) and `crates/aq-server/Cargo.toml` (dependency).
- `build-budget.toml` (lowered).

**aq-worker**
- `crates/aq-machine-load/` (new crate):
  - `Cargo.toml`
  - `src/lib.rs` (types, config, sampler, loop)
  - `src/proc.rs` (/proc parsers)
  - `src/host.rs` (Windows host-load file)
  - `src/tests.rs`
- `protocol/pinned/worker-machine/v1/...` and `protocol/pinned/worker-machine/source.json` (new pin).
- `crates/aq-worker/src/client.rs`: `post` becomes `pub`.
- `crates/aq-worker/src/main.rs`: spawns the report loop in `Cmd::Run`.
- `crates/aq-worker/Cargo.toml` and the workspace `Cargo.toml`.
- `crates/aq-worker/tests/client.rs`: the post test.
- `packaging/windows/aq-host-load.ps1` (new).
- `packaging/worker/aq-worker-ctl`: the mount and the `machine-config` command.
- `docs/operations/install-worker.md`: the Windows probe section.

**aq-dashboard**
- `scripts/pin-contract.py` (new).
- `contracts/pinned/dashboard-read/**` and `contracts/pinned/source.json`.
- `src/api/wire/swarm.ts` and `src/api/adapters/swarm.ts` (+ `.test.ts`).
- `src/features/agents/swarm/`:
  - `graph.ts` (+ `graph.test.ts`)
  - `SwarmPage.tsx`
  - `SwarmTree.tsx`
  - `SwarmCard.tsx`
  - `SwarmPane.tsx`
  - `AgentPane.tsx`
  - `useAgentTokens.ts`
  - `Swarm.module.css`
  - `SwarmPage.test.tsx`
  - `SwarmPane.test.tsx`
  - `index.tsx`
- `src/features/agents/AgentsTabs.tsx` and `AgentsTabs.module.css`.
- `src/app/routes.tsx`, `src/app/compatibility.ts`.
- `src/features/agents/Agents.tsx`, `src/features/overview/PinnedSessions.tsx`.
- `src/api/interactiveTerminalClient.ts` (+ test), `src/features/agents/InteractiveTerminal.tsx` and `.module.css`.
- `src/mocks/swarm.ts`, `src/mocks/handlers.ts`, `src/mocks/scenarios.ts`.
- `src/test/reactFlow.ts`.
- The tests that open `/agents` for the roster (listed in D3).
- `tests/swarm.spec.ts` (new), `tests/visual/live.spec.ts`.
- `docs/design/{swarm-tree-v3.html,14-swarm-tree-v3.png,manifest.json,visual-acceptance.md}`, `docs/specs/0002-product-design.md`.
- `package.json` and `pnpm-lock.yaml`.

---

## Read model (contract `dashboard-read-1`, `GET /api/v1/swarm`)

Every struct derives `Clone, Debug, Serialize, Deserialize, JsonSchema` and has `#[serde(deny_unknown_fields)]`. Every enum derives `Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq` with `#[serde(rename_all = "snake_case")]`. `Timestamp`, `EntityRef`, `TitledRef`, `ProjectRef`, `Revision`, `WorkerHealth` and `NodeSlots` are the existing contract types.

```rust
pub struct Swarm { pub server: SwarmServer, pub machines: Vec<SwarmMachine>, pub waiting: SwarmWaiting }
pub struct SwarmServer { pub version: String, pub source_revision: String, pub running_agents: u32 }
pub enum MachineKind { Local, Cloud }
pub enum MachineState { Online, Holding, Offline, Stopped, Booting }
pub struct SwarmMachine {
    pub id: String,                       // machine key, or "node:<uuid>"
    pub label: String,
    pub kind: MachineKind,
    pub state: MachineState,
    pub holding: Vec<HoldReason>,
    pub last_seen_at: Option<Timestamp>,  // newest node heartbeat
    pub load: Option<MachineLoad>,        // None when offline or unreported
    pub hardware: Option<MachineHardware>,
    pub agent_max: Option<u32>,
    pub cost: Option<MachineCost>,        // phase 4; always None now
    pub nodes: Vec<SwarmNode>,
}
pub enum LoadSource { Linux, WindowsHost }
pub enum HostProbe { NotApplicable, Fresh, Stale, Missing }
pub struct MachineLoad {
    pub source: LoadSource, pub host_probe: HostProbe,
    pub cpu_busy_bps: u16, pub memory_used_bps: u16,
    pub cpu_avg_15m_bps: u16, pub memory_avg_15m_bps: u16, pub samples_15m: u32,
    pub observed_at: Timestamp, pub fresh: bool,
}
pub struct MachineHardware { pub logical_cpus: u32, pub memory_total_bytes: u64,
    pub disk_total_bytes: Option<u64>, pub disk_free_bytes: Option<u64> }
pub struct MachineCost { pub currency: String, pub micros_per_hour: u64, pub started_at: Timestamp }
pub enum ServiceState { Running, Stopped, Draining, Unreported }
pub struct WorkerService { pub state: ServiceState, pub since: Option<Timestamp> }
pub enum ReportedState { Ready, Unavailable, Unreported }
pub struct SwarmNode {
    pub node: EntityRef,                  // type "worker"
    pub name: String,
    pub health: WorkerHealth,
    pub service: WorkerService,
    pub backend_kind: Option<String>,     // "docker-local" | "boat" | ...
    pub worker_version: String,
    pub sandbox_templates: Vec<String>,   // "sha256:<64 hex>"
    pub agent_channel: ReportedState,     // always Unreported: no node reports it yet
    pub last_seen_at: Option<Timestamp>,
    pub slots: NodeSlots,
    pub holding: Vec<HoldReason>,
    pub agents: Vec<SwarmAgent>,
}
pub enum IntelligenceClass { Fast, Standard, Deep }
pub struct SwarmAgent {
    pub session_id: Uuid, pub attempt_id: Uuid,
    pub task: Option<TitledRef>, pub project: ProjectRef,
    pub provider: Option<String>, pub model: Option<String>, pub harness: Option<String>,
    pub intelligence_class: Option<IntelligenceClass>,
    pub terminal: Option<EntityRef>,
    pub started_at: Option<Timestamp>,
}
pub struct SwarmWaiting { pub reasons: Vec<HoldReason>, pub agents: Vec<WaitingAgent> }
pub struct WaitingAgent {
    pub task: TitledRef, pub project: ProjectRef,
    pub provider: Option<String>, pub intelligence_class: Option<IntelligenceClass>,
    pub reasons: Vec<HoldReason>, pub since: Option<Timestamp>,
}
pub enum HoldCode {
    WorkerOffline, WorkerRevoked, WorkerStopped, WorkerDraining,
    CapabilityUnreported, CapabilityExpired,
    MeasurementExpired, MeasurementUnavailable,
    NodeSpendCeilingReached, NodeSpendUnaccounted, NodeDiskPressure,
    CpuAboveThreshold, MemoryAboveThreshold, AgentMaximumReached,
    NoMachineOnline, NoEligibleMachine, NoFreeSlot,
}
pub struct HoldReason { pub code: HoldCode, pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub remedy: Option<String> }
```

**Hold reason copy** (exact; `message` / `remedy`):

| Code | message | remedy |
| --- | --- | --- |
| `worker_offline` | Worker is offline | Start the worker on this machine |
| `worker_revoked` | Worker credential is revoked | Enroll the machine again |
| `worker_stopped` | Worker is stopped | Start the worker |
| `worker_draining` | Worker is draining | Wait for running agents to finish, or start the worker again |
| `capability_unreported` | Worker has not reported its capabilities | Check that the worker runs the current version |
| `capability_expired` | Capability report expired | Wait for the next report; restart the worker if none arrives |
| `measurement_expired` | Context measurement expired | Measure this worker's runtime again |
| `measurement_unavailable` | No context measurement for this worker's runtime | Measure this worker's runtime |
| `node_spend_ceiling_reached` | Spend ceiling reached | Raise the node's spend ceiling |
| `node_spend_unaccounted` | Spend is not accounted yet | Wait for usage accounting to catch up |
| `node_disk_pressure` | Disk is above 85% used | Free disk space on this machine |
| `no_machine_online` | No machine is online | Start a worker on any machine |
| `no_eligible_machine` | No machine can take work: `{label}: {first message}`, joined by `; ` | the first eligible-blocking remedy |
| `no_free_slot` | Every slot on an eligible machine is busy | Wait for a running agent to finish, or add a machine |

---

## Merge order

1. **aq-server `swarm-p2/server`** (Tasks S1–S5) and **aq-dashboard `swarm-p2/dashboard`** (D1–D7) are developed together. D1 pins the server PR head.
2. **Merge them together (Task M1):**
   - merge the server PR first;
   - re-pin the dashboard to the server **merge commit** at once, and confirm the digests are unchanged and only `commit` moved;
   - merge the dashboard PR next, with no other aq-server `protocol/dashboard-read` change merged in between.
3. **aq-worker `swarm-p2/worker`** (W1–W3) merges after the server PR, pinning `protocol/worker-machine/v1` from the server merge commit.
   - An old server answers the new route with 404. The worker loop logs and keeps its interval, so the worker never breaks against an older server.
4. **Task A1**, live acceptance on aq-ms1, runs after all three are on main.

Task order inside the program:
- S1 → S2 → S3 → S4 → S5.
- W1 needs S3's protocol fixtures; then W2 and W3.
- D1 needs S5's schema and fixture; then D2, D3 and D4 in any order; D5 needs D2; D6 needs D4 and D5; D7 needs D6.
- Then M1 and A1.

---

## aq-server tasks

### Task S1: Swarm contract types

**Files:**
- Create: `crates/aq-dashboard-contract/src/swarm_contract.rs`
- Modify: `crates/aq-dashboard-contract/src/lib.rs` (module lines 2-3; `Response` enum near line 791)
- Modify (generated): `protocol/dashboard-read/v1/schema.json`

**Interfaces:**
- Produces: every type in "Read model" above, exported from `aq_dashboard_contract::*`, plus `Response::Swarm(Box<Envelope<Swarm>>)`.

- [ ] **Step 1: Write the failing test.** Append to `swarm_contract.rs` a `#[cfg(test)] mod tests` containing:

```rust
#[test]
fn swarm_enums_serialize_snake_case() {
    assert_eq!(serde_json::to_value(MachineState::Holding).unwrap(), "holding");
    assert_eq!(serde_json::to_value(LoadSource::WindowsHost).unwrap(), "windows_host");
    assert_eq!(serde_json::to_value(HostProbe::NotApplicable).unwrap(), "not_applicable");
    assert_eq!(serde_json::to_value(ServiceState::Unreported).unwrap(), "unreported");
    assert_eq!(serde_json::to_value(HoldCode::NoFreeSlot).unwrap(), "no_free_slot");
    assert_eq!(serde_json::to_value(IntelligenceClass::Deep).unwrap(), "deep");
}
#[test]
fn hold_reason_omits_absent_remedy_and_refuses_unknown_fields() {
    let r = HoldReason { code: HoldCode::WorkerOffline, message: "Worker is offline".into(), remedy: None };
    assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::json!({"code":"worker_offline","message":"Worker is offline"}));
    assert!(serde_json::from_value::<HoldReason>(serde_json::json!({"code":"worker_offline","message":"m","extra":1})).is_err());
}
#[test]
fn schema_lists_the_swarm_response() {
    assert!(crate::schema().to_string().contains("SwarmMachine"));
}
```

- [ ] **Step 2: Run it and confirm it fails.**
  - Run: `timeout 900 cargo nextest run -p aq-dashboard-contract`
  - Expected: compile errors, because the types do not exist yet.

- [ ] **Step 3: Implement the types.**
  - Write every type from "Read model" in `swarm_contract.rs`, using `use crate::{EntityRef, NodeSlots, ProjectRef, Timestamp, TitledRef, WorkerHealth};` and `use uuid::Uuid;`.
  - Add one doc comment per struct, taken from the field comments above.
  - In `lib.rs`, add `pub mod swarm_contract;` and `pub use swarm_contract::*;` after line 3.
  - Add `Swarm(Box<Envelope<Swarm>>),` to `Response` after `Capacity(...)`.

- [ ] **Step 4: Run the tests and regenerate the schema.**
  - Run: `timeout 900 cargo nextest run -p aq-dashboard-contract`. Expected: 3 passed.
  - Run: `cargo run -p aq-server --example dashboard_contract`, then `cargo run -p aq-server --example dashboard_contract -- --check`. Expected: the second command exits 0.
  - Run: `python3 protocol/dashboard-read/v1/check.py`. Expected: it ends without an AssertionError.

- [ ] **Step 5: Commit.**

```bash
git add crates/aq-dashboard-contract protocol/dashboard-read/v1/schema.json
git commit -m "feat(dashboard-contract): swarm read model types (spec 0007 phase 2)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task S2: Create `aq-swarm` and move the capacity assembly out of the aq-server lib

This is a pure refactor. The `/api/v1/capacity` output must be byte-for-byte unchanged.

**Files:**
- Create: `crates/aq-swarm/Cargo.toml`, `crates/aq-swarm/src/lib.rs`, `crates/aq-swarm/src/capacity.rs`
- Modify: the root `Cargo.toml` (`members` gains `"crates/aq-swarm"`); `crates/aq-server/Cargo.toml` (`aq-swarm = { path = "../aq-swarm", version = "=0.1.0" }`)
- Modify: `crates/aq-server/src/dashboard_api/capacity.rs` (lines 24-50 and 115-326 shrink); `crates/aq-server/src/dashboard_api/errors.rs`
- Modify: `build-budget.toml` (lowered through `--update`)
- Test: the existing `it_dashboard` `dashboard_aggregation::capacity_reports_slots_eligibility_availability_and_storage`, plus unit tests in `crates/aq-swarm/src/tests.rs`

**Interfaces:**
- Produces (in `aq_swarm`):
  - `pub struct TimeError;` (derive `Debug`)
  - `pub fn timestamp(ms: u64) -> Result<Timestamp, TimeError>`: the same formatting as `service::timestamp` (`DateTime::<Utc>::from_timestamp_millis` and `to_rfc3339_opts(SecondsFormat::Millis, true)`)
  - `pub fn exact(value: u32) -> Total`
- Produces (in `aq_swarm::capacity`):
  - `pub struct Snapshot { pub observed_at_ms: Option<u64>, pub expires_at_ms: u64, pub credential_bindings: Vec<Binding> }`, with the same serde attributes as today (`#[serde(default)]` on the two defaulted fields; `Deserialize` only)
  - `pub struct Binding { pub credential_profile: ArtifactRef, pub available: bool, pub observed_at_ms: u64, pub expires_at_ms: u64 }`
  - `pub fn ineligible_reason(ceiling: NodeCeiling) -> IneligibleReason`
  - `pub struct NodeInput<'a> { pub id: Uuid, pub name: &'a str, pub health: WorkerHealth, pub last_seen_ms: Option<u64>, pub backend_kind: Option<String>, pub total: Option<u32>, pub occupied: u32, pub ceiling: Option<NodeCeiling>, pub snapshot: Option<&'a Snapshot>, pub storage: Option<NodeStorage> }`
  - `pub fn node_capacity(input: NodeInput<'_>, now_ms: u64) -> Result<NodeCapacity, TimeError>`
  - `pub fn slot_summary(nodes: &[NodeCapacity], occupied_all: u32, truncated: bool, now_ms: u64) -> Result<(SlotSummary, Option<String>), TimeError>`
  - `pub struct AccountInput { pub provider: String, pub account_reference: String, pub enabled: bool, pub profiles: Vec<ArtifactRef> }`
  - `pub fn provider_availability(accounts: Vec<AccountInput>, live: &[&Snapshot], now_ms: u64) -> Result<Vec<ProviderAvailability>, TimeError>`
- Consumes: `aq_dashboard_contract::*`, `aq_selection::{ArtifactRef, NodeCeiling}`.

`crates/aq-swarm/Cargo.toml`:

```toml
[package]
name = "aq-swarm"
version = "0.1.0"
edition.workspace = true
publish = false
description = "Swarm read model (spec 0007): capacity assembly, machine reports, swarm assembly"

[dependencies]
aq-dashboard-contract = { path = "../aq-dashboard-contract", version = "=0.1.0" }
aq-selection = { path = "../aq-selection", version = "=0.1.4" }
chrono.workspace = true
schemars = { version = "1", features = ["uuid1"] }
serde.workspace = true
serde_json.workspace = true
sqlx.workspace = true
sqlx-postgres.workspace = true
uuid.workspace = true
```

- [ ] **Step 1: Write the failing unit tests** in `crates/aq-swarm/src/tests.rs` (`#[cfg(test)] mod tests;` in `lib.rs`):

```rust
use crate::capacity::*;
use aq_dashboard_contract::*;
#[test]
fn timestamp_formats_millis_utc() {
    let expected = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1_794_000_000_123).unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert_eq!(crate::timestamp(1_794_000_000_123).unwrap().0, expected);
    assert!(expected.ends_with(".123Z"));
}
#[test]
fn node_capacity_offline_unreported_lists_both_reasons() {
    let n = node_capacity(NodeInput { id: uuid::Uuid::nil(), name: "n", health: WorkerHealth::Offline, last_seen_ms: None,
        backend_kind: None, total: Some(2), occupied: 0, ceiling: None, snapshot: None, storage: None }, 1_000).unwrap();
    assert_eq!(n.eligibility.reasons, vec![IneligibleReason::WorkerOffline, IneligibleReason::CapabilityUnreported]);
    assert_eq!(n.slots.idle, Some(2));
}
#[test]
fn slot_summary_unknown_when_an_online_node_has_no_total() { /* one Online node total None: total and idle == Total::unknown(), reason == Some("Some online workers advertise no slot total") */ }
#[test]
fn provider_unknown_without_fresh_report() { /* one enabled account, no live snapshots: state Unknown, reason NoFreshNodeReport, available_nodes 0 */ }
```

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `timeout 900 cargo nextest run -p aq-swarm`
  - Expected: compile errors.

- [ ] **Step 3: Move the code.**
  - Cut `Snapshot`, `Binding`, `reason` (renamed `ineligible_reason`) and `exact` out of `capacity.rs` into `aq_swarm`.
  - Move the body of the per-node loop that builds `reasons`, `node_idle`, `Eligibility` and the `NodeCapacity` literal (current lines 115-199, minus the `storage` lookup) into `node_capacity`.
  - Move the gaps/`SlotSummary` block (lines 200-231) into `slot_summary`. It recomputes `total`, `idle` and `unadvertised` from `nodes` with `health == Online`, using `slots.total`/`slots.idle`, and keeps the gap strings verbatim.
  - Move the loop of `providers()` into `provider_availability`.
  - What stays in `capacity.rs`:
    - the SQL;
    - `worker_status` → `WorkerHealth`;
    - the `NodeStorage` build;
    - building `AccountInput`s from `provider_accounts::project(registry, now).accounts`, with `profiles` from `registry.credentials` filtered exactly as today;
    - the calls into `aq_swarm`.
  - Add `impl From<aq_swarm::TimeError> for Error { fn from(_: aq_swarm::TimeError) -> Self { Error::Unavailable } }` to `dashboard_api/errors.rs`.

- [ ] **Step 4: Run the tests.**
  - Run: `timeout 900 cargo nextest run -p aq-swarm`. Expected: 4 passed.
  - Run: `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_aggregation::/)'`. Expected: all pass.
  - Run: `AQ_DASHBOARD_WRITE_FIXTURES=1 timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(=dashboard_aggregation::capacity_reports_slots_eligibility_availability_and_storage)' && git diff --stat protocol/dashboard-read/v1/fixtures/capacity.json`. Expected: the diff shows only `request_id`/time fields changed, or none. If more changed, the move altered behaviour: fix it, then `git checkout protocol/dashboard-read/v1/fixtures/capacity.json`.
  - Run: `python3 scripts/build-budget.py`. Expected: the script prints OK and `aq-server lib` is at least 150 lines below 109593 (S3 and S5 add about 150). Then run `python3 scripts/build-budget.py --update`. If it frees fewer than 150 lines, stop and report to the coordinator. Do not raise the ratchet.

- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml Cargo.lock crates/aq-swarm crates/aq-server/Cargo.toml crates/aq-server/src/dashboard_api build-budget.toml
git commit -m "refactor(capacity): move capacity assembly into the new aq-swarm crate

No behaviour change: /api/v1/capacity output is unchanged. Frees aq-server
lib lines for the swarm read (spec 0007 phase 2); the ratchet is lowered.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task S3: Machine reports (wire contract, storage, worker route)

**Files:**
- Create: `crates/aq-swarm/src/machine.rs`, `crates/aq-swarm/src/service.rs`
- Create: `crates/aq-server/migrations/0100_swarm_machine_reports.sql`
- Create: `protocol/worker-machine/v1/{README.md,schema.json,check.py,fixtures/report-linux.json,fixtures/report-windows-host.json}`
- Modify: `crates/aq-server/src/worker_api.rs` (route list near line 24-29; new handler after `heartbeat`)
- Modify: `scripts/ms1-demo.sh` (`cmd_node`, after the `node.json` heredoc)
- Test: `crates/aq-server/tests/it_access/worker_api.rs`, `crates/aq-swarm/src/tests.rs`

**Interfaces:**
- Produces (in `aq_swarm::machine`):
  - `pub struct MachineIdentity { #[serde(default, skip_serializing_if="Option::is_none")] pub key: Option<String>, #[serde(default, skip_serializing_if="Option::is_none")] pub label: Option<String>, pub kind: MachineKind }`
  - `pub struct MachineReport { pub schema_version: u32, pub machine: MachineIdentity, pub observed_at_ms: u64, pub source: LoadSource, pub host_probe: HostProbe, pub logical_cpus: u32, pub memory_total_bytes: u64, pub memory_used_bytes: u64, pub cpu_busy_bps: u16, #[serde(default, skip_serializing_if="Option::is_none")] pub disk_total_bytes: Option<u64>, #[serde(default, skip_serializing_if="Option::is_none")] pub disk_free_bytes: Option<u64> }`. Both structs derive `Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq` with `deny_unknown_fields`, and use the contract enums `MachineKind`, `LoadSource` and `HostProbe`.
  - `pub fn validate(&self) -> Result<(), &'static str>`
  - `pub fn memory_used_bps(&self) -> u16`
  - `pub const SAMPLE_WINDOW_MS: u64 = 900_000;`
  - `pub const FRESH_MS: u64 = 60_000;`
  - `pub async fn record(pool: &sqlx::PgPool, node_id: Uuid, report: &MachineReport, received_at_ms: u64) -> sqlx::Result<()>`
  - `pub struct MachineFacts { pub report: MachineReport, pub received_at_ms: u64, pub cpu_avg_15m_bps: u16, pub memory_avg_15m_bps: u16, pub samples_15m: u32 }`
  - `pub async fn latest(conn: &mut sqlx::PgConnection, nodes: &[Uuid], now_ms: u64) -> sqlx::Result<BTreeMap<Uuid, MachineFacts>>`
- Produces (in `aq_swarm::service`): `pub struct ServiceFact { pub state: ServiceState, pub since_ms: u64 }` and `pub async fn states(_conn: &mut sqlx::PgConnection, _nodes: &[Uuid]) -> sqlx::Result<BTreeMap<Uuid, ServiceFact>>`, whose body is `Ok(BTreeMap::new())` with the doc comment "Phase 1 (spec 0007) fills this from the worker service state."
- Produces (HTTP): `POST /v1/worker/machine` (node bearer credential, JSON `MachineReport`) → `200 {"recorded": true}`. Bad input gets `400` from `bad("invalid machine report: <reason>")`; an unknown or revoked credential gets the same errors as heartbeat.

`validate` rules, in order; each returns the quoted reason:
1. `schema_version == 1` → else "schema_version";
2. key, if present: 1–63 chars, `[a-z0-9-]`, first char `[a-z0-9]` → "key";
3. label, if present: 1–64 chars with no `char::is_control` → "label";
4. `1 <= logical_cpus <= 4096` → "logical_cpus";
5. `memory_total_bytes > 0 && memory_used_bytes <= memory_total_bytes` → "memory";
6. `cpu_busy_bps <= 10000` → "cpu_busy_bps";
7. the disk fields are both present or both absent, and free ≤ total → "disk".

`memory_used_bps` = `(used as u128 * 10000 / total as u128) as u16`.

Migration `0100_swarm_machine_reports.sql`:

```sql
-- Spec 0007 phase 2: latest machine report per node and a 15-minute load
-- window for the swarm read. received_at_ms is the server clock.
CREATE TABLE node_machine_reports (
 node_id uuid PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
 report jsonb NOT NULL CHECK (octet_length(report::text) <= 2048),
 received_at_ms bigint NOT NULL CHECK (received_at_ms > 0)
);
CREATE TABLE node_machine_samples (
 node_id uuid NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
 received_at_ms bigint NOT NULL,
 cpu_busy_bps smallint NOT NULL CHECK (cpu_busy_bps BETWEEN 0 AND 10000),
 memory_used_bps smallint NOT NULL CHECK (memory_used_bps BETWEEN 0 AND 10000),
 PRIMARY KEY (node_id, received_at_ms)
);
UPDATE meta SET value='100' WHERE key='state_format_version';
```

`record` runs one transaction:
1. upsert `node_machine_reports` (`ON CONFLICT(node_id) DO UPDATE SET report=EXCLUDED.report, received_at_ms=EXCLUDED.received_at_ms`);
2. insert a sample (`ON CONFLICT DO NOTHING`);
3. `DELETE FROM node_machine_samples WHERE node_id=$1 AND received_at_ms < $2`, with `$2 = received_at_ms - SAMPLE_WINDOW_MS`.

`latest` runs:

```sql
SELECT r.node_id, r.report, r.received_at_ms,
  COALESCE(round(avg(s.cpu_busy_bps)),0)::int AS cpu_avg,
  COALESCE(round(avg(s.memory_used_bps)),0)::int AS mem_avg,
  count(s.node_id)::int AS samples
FROM node_machine_reports r
LEFT JOIN node_machine_samples s ON s.node_id=r.node_id AND s.received_at_ms >= $2
WHERE r.node_id = ANY($1)
GROUP BY r.node_id, r.report, r.received_at_ms
```

with `$2 = now_ms - SAMPLE_WINDOW_MS`. Rows whose report fails to deserialize are skipped.

Fixtures (exact; also used by aq-worker):

`report-linux.json`:
```json
{"schema_version":1,"machine":{"key":"laptop","label":"laptop","kind":"local"},"observed_at_ms":1794100000000,"source":"linux","host_probe":"not_applicable","logical_cpus":8,"memory_total_bytes":17179869184,"memory_used_bytes":6871947674,"cpu_busy_bps":2350,"disk_total_bytes":512110190592,"disk_free_bytes":215680000000}
```

`report-windows-host.json`:
```json
{"schema_version":1,"machine":{"key":"desktop","label":"desktop","kind":"local"},"observed_at_ms":1794100000000,"source":"windows_host","host_probe":"fresh","logical_cpus":24,"memory_total_bytes":68719476736,"memory_used_bytes":39857296998,"cpu_busy_bps":6100,"disk_total_bytes":2000398934016,"disk_free_bytes":315680000000}
```

Write them pretty-printed (2-space indent, trailing newline).

- [ ] **Step 1: Write the failing tests.**
  - **Unit tests** in `aq-swarm/src/tests.rs`:
    - `machine_fixtures_round_trip_and_validate`: read both fixtures with `include_str!("../../../protocol/worker-machine/v1/fixtures/report-linux.json")` and the Windows one, deserialize, `validate()` is Ok, and re-serializing gives an equal `serde_json::Value`.
    - `machine_validate_rejects_each_rule`: one case per rule above, asserting the exact reason string.
    - `memory_used_bps_rounds_down`: total 3 / used 1 → 3333.
    - `worker_machine_schema_matches_protocol`: compare `serde_json::to_string_pretty(&schemars::schema_for!(MachineReport))` + `"\n"` with `protocol/worker-machine/v1/schema.json`. When `AQ_WRITE_PROTOCOL=1` is set, write the file instead.
  - **Integration tests** in `tests/it_access/worker_api.rs` (use the existing `harness`, `enroll` and `send`, and advance `h.now`):
    - `machine_report_records_latest_and_window`: enroll, then POST `report-windows-host.json` twice, 15 s apart, with `cpu_busy_bps` 6100 then 6300. Expect 200 `{"recorded":true}`. Then `aq_swarm::machine::latest(&mut conn, &[node_id], now_ms)` returns `cpu_avg_15m_bps == 6200`, `samples_15m == 2`, and `report.cpu_busy_bps == 6300`.
    - `machine_samples_older_than_window_are_pruned`: POST, advance 16 min, POST again. Expect `samples_15m == 1` and a direct `SELECT count(*) FROM node_machine_samples` of `1`.
    - `machine_report_rejects_invalid_and_unknown_fields`: `cpu_busy_bps: 10001` → 400; an added field `"extra":1` → 400 or 422 (assert `status.is_client_error()`).
    - `machine_report_requires_credential`: no bearer → 401.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `timeout 900 cargo nextest run -p aq-swarm` and `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::machine_/)'`
  - Expected: compile errors or 404.

- [ ] **Step 3: Implement.**
  - Write `machine.rs` and `service.rs` (with `pub mod machine; pub mod service;` in `lib.rs`).
  - Write the migration.
  - In `worker_api.rs`, add `.route("/v1/worker/machine", post(machine))` and:

```rust
async fn machine(State(state): State<AppState>, headers: HeaderMap, ApiJson(report): ApiJson<aq_swarm::machine::MachineReport>) -> Result<Json<serde_json::Value>, AppError> {
    report.validate().map_err(|r| bad(&format!("invalid machine report: {r}")))?;
    let node = authenticate(&state, &headers).await?;
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    aq_swarm::machine::record(&state.store.pool, node.id, &report, now).await.map_err(|_| AppError::Internal)?;
    Ok(Json(serde_json::json!({"recorded": true})))
}
```

    Match `bad`'s actual parameter type: if it takes `&str`, pass `&format!(..)`; if it takes `String`, pass `format!(..)`.
  - Generate the schema: `AQ_WRITE_PROTOCOL=1 timeout 900 cargo nextest run -p aq-swarm -E 'test(=tests::worker_machine_schema_matches_protocol)'`.
  - Write `protocol/worker-machine/v1/check.py`: jsonschema-validate both fixtures; assert that `cpu_busy_bps: 10001`, `source: "windows"` and an extra top-level field are invalid; print `worker-machine-1: 2 fixtures and 3 invalid cases checked`.
  - Write `README.md` (route, auth, cadence of 15 s, field meanings and units, the 60 s freshness and 15-minute window, and "the worker pins this directory by digest").
  - In `scripts/ms1-demo.sh` `cmd_node`, after the `node.json` heredoc, write `$W/machine.json`:

```bash
  host_load=null
  if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null; then host_load='"/mnt/c/ProgramData/agent-q/host-load.json"'; fi
  cat > "$W/machine.json" <<EOF
{"key": "desktop", "label": "desktop", "kind": "local", "host_load_file": $host_load}
EOF
```

- [ ] **Step 4: Run the tests.**
  - Run: `timeout 900 cargo nextest run -p aq-swarm`. Expected: all pass.
  - Run: `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^worker_api::/)'`. Expected: all pass.
  - Run: `timeout 900 cargo nextest run -p aq-server --test it_schema`. Expected: all pass (migration catalog, idempotence; retrospective W81).
  - Run: `python3 protocol/worker-machine/v1/check.py && python3 scripts/migration_catalog.py`. Expected: the check line, then `100`.
  - Run: `python3 scripts/build-budget.py`. Expected: OK.

- [ ] **Step 5: Commit.**

```bash
git add crates/aq-swarm crates/aq-server/migrations/0100_swarm_machine_reports.sql crates/aq-server/src/worker_api.rs crates/aq-server/tests/it_access/worker_api.rs protocol/worker-machine scripts/ms1-demo.sh
git commit -m "feat(swarm): worker machine reports with a 15-minute load window

POST /v1/worker/machine stores the latest report and load samples
(migration 0100). Contract in protocol/worker-machine/v1 (spec 0007 §6).

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task S4: Pure swarm assembly

**Files:**
- Create: `crates/aq-swarm/src/assemble.rs`, `crates/aq-swarm/src/class.rs`
- Test: `crates/aq-swarm/src/tests.rs`

**Interfaces:**
- Produces:
  - `pub fn intelligence_class(model: &str) -> Option<IntelligenceClass>`
  - `pub struct MeasuredWindow { pub runtime: ArtifactRef, pub qualification: ArtifactRef, pub observed_at_ms: u64, pub expires_at_ms: u64 }`
  - `pub fn measurement(facts: &[MeasuredWindow], registered: Option<(&ArtifactRef, &ArtifactRef)>, now_ms: u64) -> Option<HoldCode>`
  - `pub struct NodeExtra { pub id: Uuid, pub worker_version: String, pub sandbox_templates: Vec<String>, pub measurement: Option<HoldCode> }`
  - `pub struct SwarmInput { pub now_ms: u64, pub server: SwarmServer, pub capacity: Capacity, pub extras: Vec<NodeExtra>, pub machines: BTreeMap<Uuid, MachineFacts>, pub services: BTreeMap<Uuid, ServiceFact>, pub agents: Vec<(Uuid, SessionSummary)> }`
  - `pub fn hold(code: HoldCode) -> HoldReason` (copy from the table)
  - `pub fn assemble(input: SwarmInput) -> Result<Swarm, TimeError>`

`intelligence_class`: lower-case the model. Then, in this order:
1. contains `haiku`, `mini`, `spark` or `flash` → Fast;
2. contains `sonnet` → Standard;
3. contains `opus` → Deep;
4. otherwise None.

`measurement`:
1. `registered` None → None.
2. Matching facts = those with `runtime == r.0 && qualification == r.1`.
3. None matching → `Some(MeasurementUnavailable)`.
4. Any matching with `observed_at_ms <= now && now < expires_at_ms` → None.
5. Otherwise → `Some(MeasurementExpired)`.

`assemble` algorithm (exact):

```text
1. For each NodeCapacity n in capacity.nodes with health != Revoked (revoked nodes are dropped):
     facts   = machines.get(n.node.id); service = services.get(n.node.id)
     key     = facts.report.machine.key (Some and non-empty) else "node:<n.node.id>"
     holding = n.eligibility.reasons mapped by:
                 WorkerOffline, WorkerRevoked, NodeConcurrencyCeilingReached -> dropped
                 CapabilityUnreported -> capability_unreported; CapabilityExpired -> capability_expired
                 NodeSpendCeilingReached / NodeSpendUnaccounted / NodeDiskPressure -> same-named code
               + extras[n].measurement (if Some)
               + service.state Stopped -> worker_stopped, Draining -> worker_draining
               (only when n.health == Online; an offline node has holding = [])
     agents  = input.agents filtered by node id; SwarmAgent built from SessionSummary:
               session_id=id, attempt_id=attempt_id (skip the row when None), task, project,
               provider/model/harness from profile, class = model.and_then(intelligence_class),
               terminal, started_at = lifecycle.started_at
     SwarmNode { node, name, health, service: WorkerService{ state: service.state or Unreported,
                 since: service.since_ms -> timestamp }, backend_kind, worker_version, sandbox_templates,
                 agent_channel: Unreported, last_seen_at, slots, holding, agents }
2. Group nodes by key, preserving the capacity order (machines sorted by label, then id).
   Machine fields:
     label    = first report.machine.label, else the key without "node:" -> the node name
     kind     = first report.machine.kind, else Cloud if any backend_kind == "boat", else Local
     online   = nodes with health == Online
     state    = Offline if online is empty;
                Stopped if every online node has service.state == Stopped;
                Holding if any online node has non-empty holding;
                otherwise Online
     holding  = union of the online nodes' holding, de-duplicated by code; when the machine has more
                than one node, the message is prefixed "<node name>: "
     last_seen_at = max over nodes
     load     = None if state == Offline; otherwise from the newest-received facts among the online nodes:
                cpu_busy_bps, memory_used_bps(), the averages, samples_15m, observed_at = received_at_ms,
                fresh = now - received_at_ms <= FRESH_MS, source, host_probe
     hardware = from the same facts: logical_cpus, memory_total_bytes, disk_*
     agent_max = Some(sum of slots.total) if every node's total is Some, else None
     cost = None
3. waiting.reasons (swarm-wide):
     no machine Online/Holding/Stopped              -> [no_machine_online]
     else eligible = online nodes with empty holding and service.state != Stopped
       eligible empty -> [no_eligible_machine] with message
                         "No machine can take work: " + join("; ", "<label>: <first holding message>")
                         over the machines that have online nodes, and remedy = the first such reason's remedy
       eligible all have slots.idle == Some(0)       -> [no_free_slot]
       otherwise                                     -> []
   waiting.agents = [] (phase 3)
4. server = input.server (running_agents is the caller's count)
```

- [ ] **Step 1: Write the failing tests** in `tests.rs`, building `SwarmInput` with small helpers `node(name, health, total, occupied, reasons)` and `facts(key, cpu, received_at)`:
  - `class_table_maps_model_families`: `"claude-sonnet-5-5"` → Standard; `"claude-opus-5-5"` → Deep; `"claude-haiku-4-5"` → Fast; `"gpt-5.1-codex-mini"` → Fast; `"gpt-5-codex"` → None.
  - `measurement_unavailable_expired_and_fresh`: the three outcomes, plus `None` for an unregistered node.
  - `nodes_with_same_key_form_one_machine`: two nodes with key "desktop" → one machine with 2 nodes, `agent_max == Some(5)` for totals 3 and 2.
  - `unreported_node_is_its_own_machine`: no facts → `id == "node:<uuid>"`, label = the node name, kind Local; with `backend_kind == "boat"` → Cloud.
  - `offline_machine_omits_load_and_keeps_agents`: an Offline node with facts and one agent → state Offline, `load == None`, one agent kept, `holding == []`.
  - `holding_drops_concurrency_and_maps_measurement`: an Online node with `[NodeConcurrencyCeilingReached]` and measurement Expired → state Holding, `holding == [measurement_expired]`, message "Context measurement expired".
  - `multi_node_holding_prefixes_node_name`: the message starts with `"local-2: "`.
  - `stopped_when_every_online_node_stopped`: services Stopped on all → state Stopped, holding contains `worker_stopped`.
  - `waiting_no_machine_online`, `waiting_no_eligible_machine_names_machines` (message == `"No machine can take work: desktop: Context measurement expired"`), `waiting_no_free_slot`, and `unknown_slot_total_gives_no_free_slot_reason` (eligible node with `idle == None` → `[]`).
  - `load_freshness_boundary`: `received_at = now - 60_000` → fresh; `now - 60_001` → not fresh.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `timeout 900 cargo nextest run -p aq-swarm`
  - Expected: compile errors.

- [ ] **Step 3: Implement `class.rs` and `assemble.rs`** as specified (`pub mod assemble; pub mod class;`).

- [ ] **Step 4: Run the tests.**
  - Run: `timeout 900 cargo nextest run -p aq-swarm`
  - Expected: all pass.

- [ ] **Step 5: Commit.**

```bash
git add crates/aq-swarm
git commit -m "feat(swarm): pure swarm assembly with machine grouping, states and waiting reasons

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task S5: `GET /api/v1/swarm`, the `swarm.read` capability and the contract fixture

**Files:**
- Create: `crates/aq-server/src/dashboard_api/swarm.rs` (≤ 110 lines)
- Modify: `crates/aq-server/src/dashboard_api.rs` (`mod swarm;`; the route after `/api/v1/capacity`; a handler cloned from `capacity` at lines 262-281 with a 4 s timeout)
- Modify: `crates/aq-server/src/dashboard_api/service.rs` (capability after `capacity.read`, line ~377)
- Create: `crates/aq-server/tests/it_dashboard/dashboard_swarm.rs`; modify `tests/it_dashboard/main.rs`
- Modify: `protocol/dashboard-read/v1/{README.md,check.py}`; create `protocol/dashboard-read/v1/fixtures/swarm.json`

**Interfaces:**
- Consumes:
  - `capacity::capacity(state, runtime, user, scope) -> Result<Envelope<Capacity>>`
  - `service::{auth, fence, envelope, stamp, tx, session}`
  - `aq_swarm::{machine::latest, service::states, assemble::*}`
  - `crate::version_info()`
- Produces:
  - `pub(super) async fn swarm(state: &AppState, runtime: &Runtime, user: &User, scope: Scope) -> Result<Envelope<Swarm>>`
  - the HTTP route `GET /api/v1/swarm` (owner only; `limit`/`cursor` → 400 invalid)
  - the capability `swarm.read@1`, enabled iff `a.owner`, with the reason "The swarm requires an owner account"

`swarm()` steps (exact):
1. `let a = auth(...)?`. If `!a.owner` → `AppError::Forbidden("swarm requires owner".into())`.
2. `let cap = capacity::capacity(state, runtime, user, scope.clone()).await?;` and `let ids: Vec<Uuid>` from `cap.data.nodes`.
3. In one `tx(runtime)`, run:
   - `SELECT n.id, n.worker_version, b.registration->'backend'->'templates' AS templates, e.state->'native'->'request'->'runtime' AS runtime, e.state->'native'->'qualification' AS qualification FROM nodes n LEFT JOIN delegated_node_boots b ON b.node_id=n.id LEFT JOIN execution_nodes e ON e.node_id=n.id WHERE n.id = ANY($1)`. `templates` → each `"sha256:" + digest`. `runtime`/`qualification` → `Option<ArtifactRef>` via `serde_json::from_value(..).ok()`.
   - `aq_swarm::machine::latest(&mut tx, &ids, now)`
   - `aq_swarm::service::states(&mut tx, &ids)`
   - `SELECT session_id, node_id FROM execution_attempts WHERE holds_task AND node_id = ANY($1) ORDER BY node_id, session_id LIMIT 65`

   Then commit.
4. Measurement facts come from `state.config.profiled_execution.as_ref().map(|p| p.context_qualifications.iter().map(|q| MeasuredWindow { runtime: q.subject.runtime.clone(), qualification: q.subject.runtime_qualification.clone(), observed_at_ms: q.observed_at_ms, expires_at_ms: q.expires_at_ms }).collect()).unwrap_or_default()`.
5. For each of the first 64 sessions, call `session(state, runtime, user, id, &scope).await`:
   - `Ok(d)` → push `(node_id, d.summary)`;
   - `Err(Error::Api(AppError::Forbidden(_) | AppError::NotFound(_)))` → skip;
   - other errors → return.
6. `server = SwarmServer { version: version_info().server_version, source_revision: version_info().source_revision, running_agents: cap.data.slots.occupied.value.unwrap_or(0) }`.
7. `let data = aq_swarm::assemble::assemble(SwarmInput { .. })?;`
8. Coverage reason, collected with `;` between parts:
   - `cap.coverage.reason`;
   - `"More than 64 running agents; some are not shown"` when 65 rows came back.
9. `fence(state, runtime, user, &a).await?; envelope(state, scope, data, reason.as_deref())`.

- [ ] **Step 1: Write the failing tests** in `dashboard_swarm.rs`, modelled on `dashboard_aggregation.rs` (copy its `read` helper and the `capacity_fixture` pattern as `swarm_fixture`, writing `fixtures/swarm.json`):
  - **`swarm_requires_owner`:** create a `viewer` user as the capacity test does → GET `/api/v1/swarm` is 403.
  - **`swarm_groups_nodes_reports_load_and_lists_running_agents`:**
    - setup: `let mut f = common::profiled::fixture().await; let attempt = common::profiled::insert(&f).await;` then set `last_seen_at` to now for `f.node.node_id`, as the capacity test does;
    - insert a second node "local-2" with a `delegated_node_boots` docker-local registration (`max_concurrent_sandboxes: 2`);
    - record the Windows machine report for both nodes through `aq_swarm::machine::record` (same key "desktop");
    - GET `/api/v1/swarm` → 200;
    - assert `data.machines.len() == 1`, `machines[0].label == "desktop"`, `machines[0].nodes.len() == 2`;
    - assert `machines[0].load.source == "windows_host"` and `load.fresh == true`;
    - find the agent whose `attempt_id == attempt` and assert its `provider`/`model` equal the same session's `GET /api/v1/sessions/{id}` `profile` values;
    - assert `server.running_agents >= 1`;
    - call `swarm_fixture(&body)`.
  - **`swarm_offline_node_without_report_is_its_own_offline_machine`:** a node with `last_seen_at` NULL → `state == "offline"`, `load` null, `id == "node:<uuid>"`.
  - **`swarm_waiting_reason_when_no_machine_online`:** with every node offline, `data.waiting.reasons[0].code == "no_machine_online"`.
  - **`bootstrap_advertises_swarm_read_for_owner`:** the bootstrap capabilities contain `{"name":"swarm.read","version":"1","enabled":true}`.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_swarm::/)'`
  - Expected: 404 on the route.
  - If nextest panics with the selection-fixture gate message, add `| (binary_id(aq-server::it_dashboard) & test(/^dashboard_swarm::/))` to the `selection-fixture` filter in `.config/nextest.toml`.

- [ ] **Step 3: Implement** `swarm.rs`, the route and handler, and the capability.

- [ ] **Step 4: Run the tests, write the fixture and check the contract.**
  - Run: `timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_swarm::/) | test(/^dashboard_read::/) | test(/^dashboard_aggregation::/)'`. Expected: all pass.
  - Run: `AQ_DASHBOARD_WRITE_FIXTURES=1 timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(=dashboard_swarm::swarm_groups_nodes_reports_load_and_lists_running_agents)'`. Expected: it writes `protocol/dashboard-read/v1/fixtures/swarm.json`.
  - Extend `check.py` with: validate `swarm.json` against the schema; assert `data.machines[0].load.source == "windows_host"`; assert that `machines[0].state = "sleeping"` is invalid and that an extra field on `machines[0]` is invalid; print `swarm fixture and 2 invalid cases checked`.
  - Add a README paragraph: "`swarm.read@1` advertises owner-only GET `/api/v1/swarm` (`swarm.json`)…". It gives the machine grouping, state rules, hold codes table and `waiting` rules from this plan in prose, and ends "Nothing here grants placement."
  - Run: `python3 protocol/dashboard-read/v1/check.py`. Expected: OK.
  - Run: `cargo run -p aq-server --example dashboard_contract -- --check`. Expected: exit 0.
  - Run: `python3 scripts/build-budget.py`. Expected: OK (aq-server lib at or below the S2-lowered ratchet). If it is over, move the SQL-row-to-`NodeExtra` mapping into an `aq_swarm::assemble::NodeExtra::from_row(...)` helper until it passes. Then run `--update`.
  - Run: `cargo clippy -p aq-swarm -p aq-server --all-targets -- -D warnings && cargo fmt --check`. Expected: clean.

- [ ] **Step 5: Commit and open the PR.**

```bash
git add crates/aq-server/src/dashboard_api.rs crates/aq-server/src/dashboard_api crates/aq-server/tests/it_dashboard protocol/dashboard-read .config/nextest.toml build-budget.toml
git commit -m "feat(dashboard): GET /api/v1/swarm read model and swarm.read capability (spec 0007 phase 2)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin swarm-p2/server
gh pr create --title "Swarm phase 2: swarm read model and machine reports" --body "Spec 0007 phase 2 (server). Plan: docs/superpowers/plans/2026-11-07-swarm-p2-swarm-page.md. Merges together with the aq-dashboard re-pin (W76).

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

---

## aq-worker tasks

### Task W1: `aq-machine-load` crate

**Files:**
- Create: `crates/aq-machine-load/{Cargo.toml,src/lib.rs,src/proc.rs,src/host.rs,src/tests.rs}`
- Create: `protocol/pinned/worker-machine/v1/` (a byte copy of aq-server's `protocol/worker-machine/v1/` at the S PR head) and `protocol/pinned/worker-machine/source.json` (`{"repository":"ElectricJack/aq-server","commit":"<sha>","sha256":{"v1/README.md":"…", "v1/schema.json":"…", "v1/check.py":"…", "v1/fixtures/report-linux.json":"…", "v1/fixtures/report-windows-host.json":"…"}}`)
- Modify: the workspace `Cargo.toml` `members` (add `"crates/aq-machine-load"`)

**Interfaces:**
- Produces (types mirror the server contract; serde only, `deny_unknown_fields`, snake_case enums):
  - `MachineKind { Local, Cloud }` (`Default` = Local), `LoadSource { Linux, WindowsHost }`, `HostProbe { NotApplicable, Fresh, Stale, Missing }`
  - `MachineIdentity { key: Option<String>, label: Option<String>, kind: MachineKind }`
  - `MachineReport { ..same fields as the server.. }`
  - `pub struct MachineConfig { #[serde(default)] pub key: Option<String>, #[serde(default)] pub label: Option<String>, #[serde(default)] pub kind: MachineKind, #[serde(default)] pub host_load_file: Option<PathBuf>, #[serde(default)] pub disk_path: Option<PathBuf>, #[serde(default = "default_interval_secs")] pub interval_secs: u64 }` (default 15; `load` enforces 5..=300)
  - `impl MachineConfig { pub fn load_optional(path: &Path) -> anyhow::Result<Self> }`: a missing file gives the default; a file larger than 64 KiB, or invalid, is an error
  - in `proc.rs`: `pub struct CpuTimes { pub busy: u64, pub total: u64, pub logical_cpus: u32 }`, `pub fn parse_proc_stat(text: &str) -> Option<CpuTimes>`, `pub fn cpu_busy_bps(prev: &CpuTimes, cur: &CpuTimes) -> u16`, `pub fn parse_meminfo(text: &str) -> Option<(u64, u64)>` (total and available bytes)
  - in `host.rs`: `pub struct HostSample { pub observed_at_ms: u64, pub logical_cpus: u32, pub cpu_busy_bps: u16, pub memory_total_bytes: u64, pub memory_used_bytes: u64, pub disk_total_bytes: Option<u64>, pub disk_free_bytes: Option<u64> }`, `pub enum HostLoad { Fresh(HostSample), Stale, Missing }`, `pub const HOST_FRESH_MS: u64 = 30_000;`, `pub fn read_host_load(path: &Path, now_ms: u64) -> HostLoad`
  - `pub fn is_wsl() -> bool` (reads `/proc/sys/kernel/osrelease`, case-insensitive `microsoft`)
  - `pub struct Sampler { config: MachineConfig, state_dir: PathBuf, prev: Option<CpuTimes>, wsl: bool }`, `impl Sampler { pub fn new(config: MachineConfig, state_dir: PathBuf) -> Self; pub fn sample(&mut self, now_ms: u64) -> anyhow::Result<MachineReport> }`
  - `pub async fn report_loop<F, Fut>(mut sampler: Sampler, interval: Duration, mut send: F, shutdown: impl Future<Output = ()>) where F: FnMut(MachineReport) -> Fut, Fut: Future<Output = Result<(), String>>`

Parsing (exact):

```text
parse_proc_stat: first line starting "cpu " -> fields user nice system idle iowait irq softirq steal [guest guest_nice]
  total = user+nice+system+idle+iowait+irq+softirq+steal   (guest is already inside user)
  busy  = total - (idle + iowait)
  logical_cpus = count of lines whose prefix is "cpu" followed by an ASCII digit
cpu_busy_bps: dt = cur.total.saturating_sub(prev.total); if dt == 0 -> 0
  else min(10000, cur.busy.saturating_sub(prev.busy) * 10000 / dt)
parse_meminfo: "MemTotal:" and "MemAvailable:" values in kB * 1024; both required
read_host_load: read <= 4096 bytes; JSON {"schema":"aq-host-load-1", observed_at_ms, logical_cpus, cpu_busy_bps,
  memory_total_bytes, memory_used_bytes, disk_total_bytes?, disk_free_bytes?} (unknown fields ignored);
  missing or unparseable or wrong schema -> Missing; |now - observed_at_ms| > HOST_FRESH_MS -> Stale; else Fresh
```

`Sampler::new` takes the first `/proc/stat` reading into `prev`. `sample`:
1. Read `/proc/stat`, compute bps against `prev`, and store the new reading.
2. Read `/proc/meminfo`.
3. Get disk from `libc::statvfs` on `disk_path` or else `state_dir`: total = `f_blocks*f_frsize`, free = `f_bavail*f_frsize`; `None` on error.
4. Then:
   - **`host_load_file` set:** `Fresh(h)` → `source=WindowsHost, host_probe=Fresh`, values from `h` (disk from `h` when present, else from statvfs). `Stale` → Linux values with `host_probe=Stale`. `Missing` → Linux values with `Missing`.
   - **Not set:** Linux values, with `host_probe = Missing` if `wsl`, else `NotApplicable`.
   - `memory_used_bytes = total - available`.
   - `machine` comes from the config's key, label and kind.
   - `observed_at_ms = now_ms`.

`report_loop`:
1. Loop: `tokio::select!` on shutdown vs `sleep(interval)`.
2. Then `sampler.sample(now)`. On `Err`, `tracing::warn!` and continue.
3. Then `send(report).await`. On `Err(m)`, warn on the first failure and after every 40th consecutive failure, `debug!` otherwise. Never exit except on shutdown.

- [ ] **Step 1: Write the failing tests** in `tests.rs`:
  - **`parses_proc_stat_and_counts_cpus`:** input
    ```
    cpu  100 0 50 800 50 0 0 0 0 0
    cpu0 50 0 25 400 25 0 0 0 0 0
    cpu1 50 0 25 400 25 0 0 0 0 0
    intr 1
    ```
    gives `busy == 150, total == 1000, logical_cpus == 2`.
  - **`cpu_bps_from_two_readings`:** prev `{busy:150,total:1000}` and cur `{busy:400,total:1500}` give `5000`; equal totals give `0`.
  - **`parses_meminfo`:** `"MemTotal:       16384 kB\nMemFree: 1 kB\nMemAvailable:    4096 kB\n"` gives `(16777216, 4194304)`; missing MemAvailable gives `None`.
  - **`fresh_host_file_is_used`:** write `{"schema":"aq-host-load-1","observed_at_ms":1000,"logical_cpus":24,"cpu_busy_bps":6100,"memory_total_bytes":100,"memory_used_bytes":58}` to a tempdir and read it at `now=1500` → `Fresh` with `cpu_busy_bps 6100`.
  - **`stale_host_file_falls_back_to_linux_and_says_stale`:** the same file at `now = 1000 + 30_001` gives `Stale`. Then a `Sampler` whose config has `host_load_file` = that file gives a report with `source == Linux` and `host_probe == Stale`. The `/proc` reads are real; only assert those two fields.
  - **`missing_or_wrong_schema_is_missing`.**
  - **`machine_config_defaults_and_bounds`:** a missing file gives `interval_secs == 15` and kind Local; `interval_secs: 4` is an error.
  - **`pinned_fixtures_round_trip`:** `include_str!("../../../protocol/pinned/worker-machine/v1/fixtures/report-windows-host.json")` and the Linux one deserialize to `MachineReport` and re-serialize to an equal `serde_json::Value`.
  - **`pinned_contract_digests_match`:** for every entry in `protocol/pinned/worker-machine/source.json`, the file's SHA-256 equals the recorded digest. Use `sha2`.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `timeout 900 cargo test -p aq-machine-load --locked`
  - Expected: compile errors.

- [ ] **Step 3: Implement** the crate, with `[dependencies]` anyhow, libc, serde, serde_json, sha2 (dev), tempfile (dev), tokio (`time`, `macros`), and tracing, all `.workspace = true`. Copy the pinned contract files. Compute the digests with `sha256sum`.

- [ ] **Step 4: Run the tests and checks.**
  - Run: `timeout 900 cargo test -p aq-machine-load --locked`. Expected: all pass.
  - Run: `python3 scripts/build-budget.py`. Expected: OK, with an unchanged test-binary count (unit tests only).
  - Run: `cargo clippy -p aq-machine-load --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml Cargo.lock crates/aq-machine-load protocol/pinned/worker-machine
git commit -m "feat(machine-load): sample machine CPU/RAM/disk and Windows host load (spec 0007 §6)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task W2: Report from `aq-worker run`

**Files:**
- Modify: `crates/aq-worker/src/client.rs:138` (`async fn post` → `pub async fn post`, plus a one-line doc comment replacing no other line)
- Modify: `crates/aq-worker/src/main.rs` (`Cmd::Run` arm, around lines 792-832)
- Modify: `crates/aq-worker/Cargo.toml` (`aq-machine-load = { path = "../aq-machine-load", version = "=0.1.0" }`)
- Test: `crates/aq-worker/tests/client.rs`

**Interfaces:**
- Consumes: `aq_machine_load::{MachineConfig, Sampler, report_loop}` and `Client::post::<B, T>(&self, path: &str, body: &B, auth: bool) -> Result<T, CallError>`.

- [ ] **Step 1: Write the failing test** in `tests/client.rs`:
  - **`machine_report_posts_with_bearer`:** serve an axum route `POST /v1/worker/machine` that captures the `Authorization` header and the body and returns `{"recorded":true}`.
  - Call `client(&url).post::<_, serde_json::Value>("/v1/worker/machine", &report, true)` with `report` parsed from the pinned Windows fixture.
  - Assert the header `== "Bearer aqn_x"` and the captured body equals the fixture JSON value.

- [ ] **Step 2: Run it and confirm it fails.**
  - Run: `timeout 900 cargo test -p aq-worker --locked --all-features --test client machine_report_posts_with_bearer`
  - Expected: a compile error, because `post` is private.

- [ ] **Step 3: Implement.**
  - Make `post` `pub`.
  - In `main.rs` `Cmd::Run`, after `let client = ...`:

```rust
let machine_config = aq_machine_load::MachineConfig::load_optional(&controller_config.with_file_name("machine.json"))?;
let interval = std::time::Duration::from_secs(machine_config.interval_secs);
let sampler = aq_machine_load::Sampler::new(machine_config, config.state_dir.clone());
let machine_client = client.clone();
let mut machine_shutdown = rx.clone(); // create after the watch channel exists
let machine = tokio::spawn(aq_machine_load::report_loop(sampler, interval,
    move |report| { let c = machine_client.clone(); async move {
        c.post::<_, serde_json::Value>("/v1/worker/machine", &report, true).await.map(|_| ()).map_err(|e| e.to_string()) } },
    async move { while !*machine_shutdown.borrow() { if machine_shutdown.changed().await.is_err() { break; } } }));
```

    Move the `let (tx, rx)` line above this block. After `controller.run_with_heartbeat(...)` returns, call `machine.abort();`.

- [ ] **Step 4: Run the tests and checks.**
  - Run: `timeout 900 cargo test -p aq-worker --locked --all-features --test client`. Expected: all pass.
  - Run: `python3 scripts/build-budget.py`. Expected: OK. The lib-line count must be unchanged (the edit only changes visibility); if it grew, move the doc comment to the same line.
  - Run: `cargo clippy -p aq-worker --all-targets --all-features -- -D warnings`. Expected: clean.

- [ ] **Step 5: Commit.**

```bash
git add crates/aq-worker/src/client.rs crates/aq-worker/src/main.rs crates/aq-worker/Cargo.toml crates/aq-worker/tests/client.rs Cargo.lock
git commit -m "feat(worker): aq-worker run posts a machine report every 15 s

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task W3: Windows host-load probe and packaging

**Files:**
- Create: `packaging/windows/aq-host-load.ps1`
- Modify: `packaging/worker/aq-worker-ctl` (`container_args` lines 38-50; a new `cmd_machine_config`; the usage/dispatch case)
- Modify: `docs/operations/install-worker.md` (new section "Windows host load (WSL2)")

The probe is PowerShell 5.1-compatible. The parameters are `-Install`, `-Uninstall`, `-Status`, `-Once` and `-StartNow`, plus `[string]$OutDir = (Join-Path $env:ProgramData "agent-q")`. Core sketch (exact field names and units):

```powershell
$TaskName = "AQ host load probe"
$Out = Join-Path $OutDir "host-load.json"
function Write-Sample {
  $cpu  = (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime
  $os   = Get-CimInstance Win32_OperatingSystem
  $cpus = (Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum
  $disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$($env:SystemDrive)'"
  $total = [uint64]$os.TotalVisibleMemorySize * 1024; $free = [uint64]$os.FreePhysicalMemory * 1024
  $s = [ordered]@{ schema = "aq-host-load-1"; observed_at_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();
    logical_cpus = [int]$cpus; cpu_busy_bps = [int][Math]::Min(10000, [Math]::Max(0, [int]$cpu * 100));
    memory_total_bytes = $total; memory_used_bytes = $total - $free;
    disk_total_bytes = [uint64]$disk.Size; disk_free_bytes = [uint64]$disk.FreeSpace }
  New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
  $tmp = "$Out.tmp"; [IO.File]::WriteAllText($tmp, ($s | ConvertTo-Json -Compress)); Move-Item -Force $tmp $Out
}
# -Once: Write-Sample; exit 0
# default (the task's action): while ($true) { try { Write-Sample } catch { }; Start-Sleep -Seconds 5 }
# -Install: copy this script to (Join-Path $OutDir "aq-host-load.ps1"); Register-ScheduledTask $TaskName with
#   New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME, action powershell.exe -NoProfile -WindowStyle Hidden
#   -ExecutionPolicy Bypass -File "<copied path>", settings -ExecutionTimeLimit ([TimeSpan]::Zero)
#   -MultipleInstances IgnoreNew; with -StartNow also Start-ScheduledTask. -Uninstall: Unregister-ScheduledTask
#   -Confirm:$false and remove the copied script. -Status: print the task state and the age of host-load.json.
```

Use `Win32_PerfFormattedData_PerfOS_Processor`, not `Get-Counter`, because counter names are localized. Follow the comment-help header style of `aq-wsl-autostart.ps1`, and include the line "Run from WSL: powershell.exe -NoProfile -ExecutionPolicy Bypass -File aq-host-load.ps1 -Install -StartNow".

`aq-worker-ctl` changes:
- In `container_args`, append the following before the closing brace:
  ```bash
  if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null && [[ -d /mnt/c/ProgramData/agent-q ]]; then printf '%s\n' -v "/mnt/c/ProgramData/agent-q:/run/aq-host-load:ro"; fi
  ```
- New command `machine-config <profile> [key]`. It writes `$(cfg_dir "$p")/agent-q/machine.json` (umask 077; refuse when it exists, as `node-config` does). The key defaults to `hostname | tr 'A-Z' 'a-z' | tr -c 'a-z0-9-\n' '-' | cut -c1-63`, the label to `$(hostname)` and the kind to `"local"`. `host_load_file` is `"/run/aq-host-load/host-load.json"` on WSL, else `null`.
- Add `machine-config` to the usage text and the `case` dispatch.

- [ ] **Step 1: Write the failing check.**
  - Run: `bash -n packaging/worker/aq-worker-ctl && test -f packaging/windows/aq-host-load.ps1`
  - Expected: it fails, because the file is missing.

- [ ] **Step 2: Implement** the script, the ctl changes and the doc section. The doc section covers the install command, the file path and format, the 30 s staleness, what the page shows when the probe is missing, and `-Uninstall`.

- [ ] **Step 3: Verify on this WSL2 host.**
  - Run: `bash -n packaging/worker/aq-worker-ctl`. Expected: exit 0.
  - Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w packaging/windows/aq-host-load.ps1)" -Once && python3 -c "import json;d=json.load(open('/mnt/c/ProgramData/agent-q/host-load.json'));assert d['schema']=='aq-host-load-1' and 0<=d['cpu_busy_bps']<=10000 and d['memory_used_bytes']<=d['memory_total_bytes'];print('ok',d['logical_cpus'])"`. Expected: `ok <cpu count>`.
  - Run a reader check: `timeout 900 cargo test -p aq-machine-load --locked -- --ignored real_host_file`. First add an `#[ignore]` test `real_host_file` to W1's `tests.rs` that reads `/mnt/c/ProgramData/agent-q/host-load.json` and asserts `matches!(read_host_load(..), HostLoad::Fresh(_))` within 30 s of writing. Expected: pass right after `-Once`.

- [ ] **Step 4: Commit and open the PR** (after the S PR merged; update `protocol/pinned/worker-machine/source.json` `commit` to the server merge commit and confirm the digests are unchanged).

```bash
git add packaging docs/operations/install-worker.md crates/aq-machine-load/src/tests.rs protocol/pinned/worker-machine/source.json
git commit -m "feat(packaging): Windows host-load probe for WSL2 workers and machine.json

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin swarm-p2/worker
gh pr create --title "Swarm phase 2: machine load reporting and Windows host probe" --body "Spec 0007 phase 2 (worker). Pins aq-server protocol/worker-machine/v1. Plan: aq-server docs/superpowers/plans/2026-11-07-swarm-p2-swarm-page.md.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

---

## aq-dashboard tasks

Commands for every dashboard task:
- `pnpm install --frozen-lockfile` (once per worktree)
- `pnpm typecheck`
- `pnpm test -- <file or -t name>`
- `pnpm build`
- `pnpm test:e2e -- <spec>`

### Task D1: Pin script, re-pin `dashboard-read`, wire types and adapter

**Files:**
- Create: `scripts/pin-contract.py`
- Modify: `contracts/pinned/dashboard-read/v1/**` (copied), `contracts/pinned/dashboard-read/source.json`, `contracts/pinned/source.json`
- Create: `src/api/wire/swarm.ts`, `src/api/adapters/swarm.ts`, `src/api/adapters/swarm.test.ts`

**Interfaces:**
- Produces: `scripts/pin-contract.py <aq-server-dir> <contract> <commit>`, which:
  1. replaces `contracts/pinned/<contract>/v1/` with a copy of `<aq-server-dir>/protocol/<contract>/v1/` at `<commit>`, using `git -C <dir> archive <commit> protocol/<contract>/v1 | tar -x` into a temp dir;
  2. rewrites the `"<contract>/v1/..."` entries of `sha256` in both `contracts/pinned/source.json` and `contracts/pinned/<contract>/source.json`, setting `commit` in the per-contract file and `contract_commits[<contract>] = <commit>` in the root file;
  3. prints `pinned <contract> at <commit>: <n> files`.
- Produces: in `wire/swarm.ts`, TypeScript types mirroring the contract exactly (snake_case fields; string-literal unions for enums: `MachineState = "online"|"holding"|"offline"|"stopped"|"booting"` etc.). In `adapters/swarm.ts`:

```ts
export const SWARM_POLL_MS = 10_000;
export type HoldReasonView = { code: string; message: string; remedy: string | null };
export type LoadView = { source: "linux" | "windows_host"; hostProbe: "not_applicable" | "fresh" | "stale" | "missing";
  cpuPercent: number; memoryPercent: number; cpuAvgPercent: number; memoryAvgPercent: number; samples: number; observedAt: string; fresh: boolean };
export type AgentView = { sessionId: string; attemptId: string; task: { id: string; title: string } | null; project: { id: string; label: string };
  provider: string | null; model: string | null; harness: string | null; intelligenceClass: "fast" | "standard" | "deep" | null;
  terminalId: string | null; startedAt: string | null };
export type NodeView = { id: string; name: string; health: "online" | "offline" | "revoked" | "unknown";
  service: "running" | "stopped" | "draining" | "unreported"; serviceSince: string | null; backendKind: string | null; workerVersion: string;
  sandboxTemplates: string[]; agentChannel: "ready" | "unavailable" | "unreported"; lastSeenAt: string | null;
  slots: { total: number | null; occupied: number; idle: number | null }; holding: HoldReasonView[]; agents: AgentView[] };
export type MachineView = { id: string; label: string; kind: "local" | "cloud"; state: "online" | "holding" | "offline" | "stopped" | "booting" | "unknown";
  holding: HoldReasonView[]; lastSeenAt: string | null; load: LoadView | null;
  hardware: { logicalCpus: number; memoryTotalBytes: number; diskTotalBytes: number | null; diskFreeBytes: number | null } | null;
  agentMax: number | null; cost: { currency: string; microsPerHour: number; startedAt: string } | null; nodes: NodeView[] };
export type WaitingAgentView = { taskId: string; title: string; project: { id: string; label: string }; provider: string | null;
  intelligenceClass: "fast" | "standard" | "deep" | null; reasons: HoldReasonView[]; since: string | null };
export type SwarmView = { serverTime: string; server: { version: string; sourceRevision: string; runningAgents: number };
  machines: MachineView[]; waiting: { reasons: HoldReasonView[]; agents: WaitingAgentView[] } };
export function adaptSwarm(raw: Swarm, serverTime: string): SwarmView;
```

  Adapter rules:
  - Throw `ContractError` when `machines` or `waiting` is not an array/object, or when a machine lacks a string `id`/`label`.
  - Unknown enum strings map to: state → `"unknown"`, health → `"unknown"`, intelligence class → `null`, service → `"unreported"`.
  - Percent = `Math.round(bps / 100)`.
  - Ignore unknown fields.

- [ ] **Step 1: Re-pin.**
  - Run: `python3 scripts/pin-contract.py /home/jkern/dev/aq-platform/aq-server dashboard-read <S PR head sha>`. Expected: `pinned dashboard-read at <sha>: 27 files` (26 before, plus `swarm.json`).
  - Run: `python3 contracts/pinned/dashboard-read/v1/check.py`. Expected: OK.

- [ ] **Step 2: Write the failing tests** in `src/api/adapters/swarm.test.ts`:
  - **`pinned swarm fixture adapts`:** import `contracts/pinned/dashboard-read/v1/fixtures/swarm.json`, then assert `adaptSwarm(f.data, f.server_time)` has `machines[0].label === "desktop"`, `machines[0].load?.source === "windows_host"`, a node with an agent whose `terminalId` is a string or null, and `server.runningAgents >= 1`.
  - **`unknown enum values degrade, never throw`:** clone the fixture, set `state: "sleeping"` and `intelligence_class: "genius"` → `"unknown"` and `null`.
  - **`unknown fields are ignored`:** an extra `machines[0].future = 1` → no throw.
  - **`bps become whole percents`:** `cpu_busy_bps 6149` → 61; `6150` → 62.
  - **`missing machines array is a contract error`.**

- [ ] **Step 3: Run them and confirm they fail.**
  - Run: `pnpm test -- src/api/adapters/swarm.test.ts`
  - Expected: it fails (the module is missing).

- [ ] **Step 4: Implement** `wire/swarm.ts` and `adapters/swarm.ts`. Run `pnpm test -- src/api/adapters/swarm.test.ts src/api/adapters/resultsFeedback.test.ts && pnpm typecheck`. Expected: all pass; the digest loop in `resultsFeedback.test.ts` covers the new pins.

- [ ] **Step 5: Commit.**

```bash
git add scripts/pin-contract.py contracts/pinned src/api/wire/swarm.ts src/api/adapters/swarm.ts src/api/adapters/swarm.test.ts
git commit -m "contracts: pin dashboard-read with the swarm read (aq-server swarm-p2); swarm adapter

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task D2: Dependencies, graph builder and dagre layout

**Files:**
- Modify: `package.json` (dependencies `"@xyflow/react": "12.11.3"`, `"@dagrejs/dagre": "1.1.8"`) and `pnpm-lock.yaml`
- Create: `src/features/agents/swarm/graph.ts`, `src/features/agents/swarm/graph.test.ts`

**Interfaces:**
- Produces:

```ts
export type Selection =
  | { kind: "server" } | { kind: "machine"; machineId: string } | { kind: "node"; nodeId: string }
  | { kind: "agent"; sessionId: string } | { kind: "free"; nodeId: string; index: number }
  | { kind: "waiting" } | { kind: "waitingAgent"; taskId: string };
export type CardKind = "server" | "machine" | "node" | "agent" | "free" | "waiting" | "waitingAgent";
export interface SwarmCardData extends Record<string, unknown> {
  kind: CardKind; selection: Selection; label: string; sub: string | null; reason: string | null;
  state: MachineView["state"] | null; machineKind: "local" | "cloud" | null;
  meters: { label: "CPU" | "RAM"; percent: number }[] | null; stale: boolean;
  intelligenceClass: "fast" | "standard" | "deep" | null; provider: string | null; ariaLabel: string };
export const CARD_SIZE: Record<CardKind | "machineMeters", { width: number; height: number }> = {
  server: { width: 160, height: 48 }, machine: { width: 200, height: 52 }, machineMeters: { width: 200, height: 88 },
  node: { width: 168, height: 48 }, agent: { width: 120, height: 26 }, free: { width: 120, height: 26 },
  waiting: { width: 168, height: 48 }, waitingAgent: { width: 120, height: 26 } };
export const MAX_FREE_PILLS = 8;
export function selectionId(s: Selection): string;   // "server" | "machine:<id>" | "node:<id>" | "agent:<sessionId>" | "free:<nodeId>:<i>" | "waiting" | "waiting-agent:<taskId>"
export function showWaiting(view: SwarmView, ready: number | null): boolean;  // agents.length > 0 || (ready !== null && ready > 0 && reasons.length > 0)
export function buildSwarmGraph(view: SwarmView, opts: { host: string; ready: number | null; now: string }): { nodes: Node<SwarmCardData>[]; edges: Edge[] };
export function layoutSwarm(nodes: Node<SwarmCardData>[], edges: Edge[], rankdir?: "TB" | "LR"): Node<SwarmCardData>[];
```

Build rules (exact labels):
- **Server:** label "AQ server", sub `opts.host`.
- **Machine:**
  - label = `label`;
  - sub online/holding = `` `${kind} · ${cpus ?? "?"} cores · max ${agentMax ?? "?"} agents` ``;
  - offline = `` `${kind} · offline ${age(lastSeenAt, now)}` `` (`age` = the existing `formatAge` from `src/components/time/relative`), or `` `${kind} · offline` `` when never seen;
  - stopped = `` `${kind} · stopped` ``; booting = `` `${kind} · booting` ``;
  - reason = `` `holding: ${holding[0].message}` `` when state is "holding", else null;
  - meters = `[["CPU", cpuPercent], ["RAM", memoryPercent]]` when `load` is not null and state is not offline;
  - stale = `load !== null && !load.fresh`.
- **Offline, stopped and booting machines:** no child nodes are drawn (as in the mockup); otherwise one node card per node.
- **Node:** label `name`; sub `` `${backendKind ?? "backend unknown"} · ${slots.occupied}/${slots.total ?? "?"} slots` ``.
- **Agents:** one pill per agent, label `` `${provider ?? "agent"} · ${intelligenceClass ?? "unclassified"}` ``.
- **Free pills:** `min(slots.idle ?? 0, MAX_FREE_PILLS)` pills labelled "free". When `idle > MAX_FREE_PILLS`, the last pill reads `` `+${idle - MAX_FREE_PILLS + 1} free` ``.
- **Waiting:** when `showWaiting`, add card "waiting", sub `` `${agents.length} agents · ${reasons[0]?.message ?? ""}` `` if agents exist, else `` `${ready} ready tasks · ${reasons[0].message}` ``. Under it, one pill per waiting agent: label `` `${provider ?? "task"} · ${intelligenceClass ?? title}` ``.
- **Edges:**
  - server→machine has `animated: true` iff state ∈ {online, holding} and the machine has ≥1 agent, and `style: { strokeDasharray: "5 4" }` iff state ∈ {offline, stopped, booting};
  - machine→node and node→agent/free are plain;
  - server→waiting has `style: { strokeDasharray: "4 4", stroke: "#c7802e" }`;
  - waiting→waitingAgent has `style: { strokeDasharray: "3 3" }`.
- **Order:** machines in view order, nodes in view order, agents before free pills.

`layoutSwarm` (dagre; v1 pattern):

```ts
const g = new dagre.graphlib.Graph(); g.setDefaultEdgeLabel(() => ({}));
g.setGraph({ rankdir, nodesep: 14, ranksep: 46 });
for (const n of nodes) { const s = n.data.kind === "machine" && n.data.meters ? CARD_SIZE.machineMeters : CARD_SIZE[n.data.kind]; g.setNode(n.id, { width: s.width, height: s.height }); }
for (const e of edges) g.setEdge(e.source, e.target);
dagre.layout(g);
return nodes.map(n => { const p = g.node(n.id); return { ...n, position: { x: p.x - p.width / 2, y: p.y - p.height / 2 }, width: p.width, height: p.height }; });
```

- [ ] **Step 1: Install.**
  - Run: `pnpm add --save-exact @xyflow/react@12.11.3 @dagrejs/dagre@1.1.8`
  - Expected: `package.json` shows the exact versions.

- [ ] **Step 2: Write the failing tests** in `graph.test.ts`, with a local `view()` factory building a `SwarmView` (desktop: 2 nodes, totals 3/2, occupied 3/1, 4 agents; laptop offline; studio holding; vm booting):
  - **`builds the mockup tree`:** node ids include `server`, `machine:desktop`, `node:<local-1>`, four `agent:*`, one `free:<local-2>:0`, `machine:laptop`, and no node card under laptop.
  - **`machine sub and meters`:** desktop sub is `"local · 24 cores · max 5 agents"` and meters are `[{label:"CPU",percent:61},{label:"RAM",percent:58}]`.
  - **`holding reason line`:** studio reason is `"holding: Context measurement expired"`.
  - **`offline edge dashed and not animated`.**
  - **`unknown slot total draws no free pills`:** a node with `total: null, idle: null` → no `free:` ids, and sub ends `"1/? slots"`.
  - **`free pill overflow`:** `idle: 12` → 8 pills, the last labelled `"+5 free"`.
  - **`waiting branch only with ready work and reasons`:** `ready 0` → no `waiting`; `ready 3` with a reason → `waiting` sub is `"3 ready tasks · Every slot on an eligible machine is busy"`.
  - **`layout is top-down`:** after `layoutSwarm`, server.y < machine.y < node.y < agent.y; every position is finite.

- [ ] **Step 3: Run them and confirm they fail.**
  - Run: `pnpm test -- src/features/agents/swarm/graph.test.ts`
  - Expected: it fails.

- [ ] **Step 4: Implement `graph.ts`.**
  - Run: `pnpm test -- src/features/agents/swarm/graph.test.ts && pnpm typecheck`
  - Expected: pass.

- [ ] **Step 5: Commit.**

```bash
git add package.json pnpm-lock.yaml src/features/agents/swarm/graph.ts src/features/agents/swarm/graph.test.ts
git commit -m "feat(agents): swarm graph builder and dagre layout (@xyflow/react 12.11.3, dagre 1.1.8)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task D3: Routes, tabs, links and mock data

**Files:**
- Modify: `src/app/routes.tsx:29`. Replace `agents/:sessionId?` with three entries, in this order:
  - `{ path: "agents", lazy: () => import("../features/agents/swarm") }`
  - `{ path: "agents/sessions", lazy: () => import("../features/agents") }`
  - `{ path: "agents/:sessionId", lazy: () => import("../features/agents") }`
- Create: `src/features/agents/swarm/index.tsx` (`export function Component()` = `<DestinationPage id="agents"><SwarmPage /></DestinationPage>`) and a placeholder `SwarmPage.tsx` that D6 completes (renders `<AgentsTabs current="swarm" />` and the capability note).
- Create: `src/features/agents/AgentsTabs.tsx`, `AgentsTabs.module.css`
- Modify: `src/features/agents/Agents.tsx`:
  - lines 201, 457 and 484 use `"/agents/sessions"`;
  - render `<AgentsTabs current="sessions" />` above `<Roster />` in `Agents()` (line ~592), not on `SessionPage`.
- Modify: `src/features/overview/PinnedSessions.tsx:39` (`'/agents'` → `'/agents/sessions'`)
- Modify: `src/app/compatibility.ts` (`swarm: ["swarm.read"]` in `exact`)
- Modify tests, replacing the roster-landing `"/agents"` with `"/agents/sessions"`:
  - `src/features/agents/Agents.test.tsx` (all 12, including the `href` expectation at line 63);
  - `src/app/navigation.test.tsx:26,37`;
  - `src/app/liveConnection.test.tsx:138,212`;
  - `tests/qa-core.spec.ts:71`;
  - `tests/agents-qa-fixes.spec.ts:10`;
  - `tests/inspection.spec.ts:11`;
  - `tests/live/framework.spec.ts:100`;
  - leave `src/app/bundleCompatibility.test.tsx:100` and `tests/live/framework.spec.ts:13` on `/agents`.
- Modify: `tests/visual/live.spec.ts:26` (keep `/agents`; add `{ id: "agents-sessions", path: "/agents/sessions" }`)
- Create: `src/mocks/swarm.ts`; modify `src/mocks/handlers.ts` (spread `...swarmHandlers(s.reads)`) and `src/mocks/scenarios.ts` (`withActions` also appends `{ name: "swarm.read", version: "1", enabled: true }` after filtering out `swarm.read`)
- Modify: `docs/specs/0002-product-design.md:68` (Agents row: `` `/agents` (Swarm), `/agents/sessions`, `/agents/:sessionId` | Swarm tree and properties pane (spec aq-server 0007 §5); roster filters or focus ``)
- Test: `src/features/agents/swarm/SwarmPage.test.tsx` (first tests)

**Interfaces:**
- Produces:
  - `AgentsTabs({ current }: { current: "swarm" | "sessions" })`: a `<nav aria-label="Agents views">` with two `NavLink`s, "Swarm" → `/agents` and "Sessions" → `/agents/sessions`, keeping `location.search`, with `aria-current="page"` on the current one. Style it like the Playbooks tab pattern (12px uppercase is for section headings, not tabs: use `--font-size-meta`).
  - `swarmHandlers(reads: Scenario["reads"])`: `GET /api/v1/swarm`. `"failed"` → 503 `read_model_unavailable`; `"empty"` → `envelope({ ...SWARM_MOCKUP, machines: [], waiting: { reasons: [NO_MACHINE_ONLINE], agents: [] } })`; otherwise `envelope(SWARM_MOCKUP)`.
  - `SWARM_MOCKUP: Swarm` (typed by `wire/swarm.ts`), labelled `// Labelled fixture reproducing swarm-tree-v3.html; not real infrastructure.`, with these entries:
    - **server:** `{ version: "0.1.0", source_revision: "85070006aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", running_agents: 4 }`.
    - **desktop:** local, online, windows_host fresh, CPU 6100/RAM 5800 bps, averages 5500/5600, 24 cpus, 64 GiB, disk 294 GB free of 2 TB, agent_max 5.
      - node `local-1`: docker-local, 3/3 slots, agents claude·standard, claude·deep, codex·fast. Codex has `provider: "codex"`. Use `FIXTURE_SESSIONS` session ids and `fixtureTerminalId(i)` terminal ids for i = 0..3.
      - node `local-2`: docker-local, 1/2 slots, agent claude·fast, idle 1.
    - **laptop:** local, offline, `last_seen_at` two hours before `SERVER_TIME`, one offline node `laptop-1`, `load: null`.
    - **studio:** local, holding, `holding: [measurement_expired]`, node `studio-1` 0/2 idle 2, linux load CPU 2350/RAM 4000, 8 cpus.
    - **boat vm-a:** cloud, booting, no nodes, `cost: { currency: "USD", micros_per_hour: 210000, started_at: SERVER_TIME }`.
    - **waiting:** reasons `[no_free_slot]`, agents: two entries (claude·deep "Fix flaky host-cancel test", codex·fast "Bump sandbox image") with reason `no_free_slot`.

- [ ] **Step 1: Write the failing tests** in `SwarmPage.test.tsx` (MSW `setupServer(...handlersFor("default"))`, as in `Agents.test.tsx`):
  - **`agents landing is the swarm tab`:** `renderApp("/agents")` → a link "Swarm" with `aria-current="page"` and a link "Sessions" with `href="/agents/sessions"`.
  - **`sessions tab keeps the roster`:** `renderApp("/agents/sessions")` → `findByRole("list", { name: "Sessions" })`.
  - **`swarm without capability degrades to a note and keeps the Sessions tab`:** override bootstrap without `swarm.read` → text "Swarm is not available on this server." and the Sessions link is present.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `pnpm test -- src/features/agents/swarm/SwarmPage.test.tsx`
  - Expected: it fails.

- [ ] **Step 3: Implement** the routes, tabs, links, test-path updates, mocks and spec line. The placeholder `SwarmPage` shows:

```tsx
<AgentsTabs current="swarm" />
{!cap.enabled && <p role="note">Swarm is not available on this server. <Link to={{ pathname: "/agents/sessions", search }}>Open Sessions</Link></p>}
```

- [ ] **Step 4: Run the tests.**
  - Run: `pnpm test && pnpm typecheck`. Expected: the full unit suite passes, including the moved roster tests.
  - Run: `pnpm test:e2e -- tests/qa-core.spec.ts tests/agents-qa-fixes.spec.ts tests/inspection.spec.ts`. Expected: pass.

- [ ] **Step 5: Commit.**

```bash
git add -A src tests docs/specs/0002-product-design.md
git commit -m "feat(agents): Swarm becomes the Agents landing; roster moves to /agents/sessions

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task D4: Terminal opens with control taken; takeover notices

**Files:**
- Modify: `src/api/interactiveTerminalClient.ts` (options type at line 28; `TerminalState` at line 11; `inspect`, `control`, `watch`, `emit` and `takeControl`)
- Modify: `src/features/agents/InteractiveTerminal.tsx` (`TerminalPaneProps`, `TerminalPane` signature, the `connectInteractiveTerminal` call, the effect dependency list, the toolbar) and `InteractiveTerminal.module.css`
- Test: `src/api/interactiveTerminalClient.test.ts`

**Interfaces:**
- Produces:
  - option `autoControl?: boolean` on `connectInteractiveTerminal`;
  - `TerminalState.notice?: string | null`;
  - prop `autoControl?: boolean` on `TerminalPaneProps` (default false; the Sessions page does not pass it).
  - Exact notice strings:
    - `TAKEOVER_NOTICE_LOST = "Another viewer took control. You are watching."`
    - `TAKEOVER_NOTICE_TOOK = "You took control from another viewer."`
    - both exported from `interactiveTerminalClient.ts`.

Algorithm (add to the existing closure; nothing else changes):

```text
let notice: string | null = null, autoTried = false
emit():  include `notice` in the state object
inspect(): replace `if (owned && info.control.lease?.lease_id !== owned) {...}` with
           if (owned && info.control.lease?.lease_id !== owned) {
             const other = !!info.control.lease && !acquired.has(info.control.lease.lease_id);
             owned = null; queue = []; queued = 0;
             if (other) notice = TAKEOVER_NOTICE_LOST;
           }
control(kind): before the POST, when kind === 'control':
             const heldByOther = !!(info.control.lease && !acquired.has(info.control.lease.lease_id) && info.control.lease.expires_at_ms > now());
           after a successful acquire: if (kind === 'control') notice = heldByOther ? TAKEOVER_NOTICE_TOOK : null;
watch():   right after the first `await inspect();` at the top of watch():
             if (options.autoControl && options.allowControl && !autoTried) { autoTried = true; await control('control'); }
           (autoTried lives outside watch(), so run()'s automatic reconnect never re-acquires)
takeControl: () => { notice = null; return control('control'); }
```

`InteractiveTerminal` renders `{state.notice && <p role="status" className={styles.notice}>{state.notice}</p>}` as the first child of `.toolbar`. CSS:

```css
.notice { margin: 0 0 var(--space-1); padding: var(--space-1) var(--space-2); border-radius: var(--radius-control); background: color-mix(in srgb, var(--color-attention) 18%, transparent); color: var(--color-text); font-size: var(--font-size-xs); }
```

Pass `autoControl` into `connectInteractiveTerminal` and add it to the `useEffect` dependency array.

- [ ] **Step 1: Write the failing tests**, mocking `terminalJson` as in the existing "allows a fresh user operation…" test (a `lease` variable the mock mutates; `control` increments `generation` and sets a lease with a new `lease_id`):
  - **`auto control acquires once after the first info`:**
    - setup: `autoControl: true, allowControl: true`;
    - assert one `control` mutation is sent without calling `takeControl`;
    - assert `controlling` becomes true.
  - **`auto control is not retried after an automatic reconnect`:**
    - setup: make the first `tail` throw `new ApiError("unavailable", "x", 503)` (as the reconnect tests do), with `reconnectDelay: () => 0`;
    - assert there is exactly one `control` POST in total after the reconnect's info read.
  - **`auto control never runs without terminal.control`:** `allowControl: false, autoControl: true` → zero `control` POSTs; the final message is "Watch only".
  - **`lease taken by another viewer drops control and raises the notice`:**
    - setup: acquire, then flip the mock's info lease to `{ ...lease, lease_id: '10000000-0000-4000-8000-0000000000aa' }`;
    - assert the next state has `controlling === false` and `notice === TAKEOVER_NOTICE_LOST`.
  - **`taking over another viewer's lease reports it`:**
    - setup: the initial info has a foreign lease with `expires_at_ms: 15000`;
    - call `takeControl()`;
    - assert `notice === TAKEOVER_NOTICE_TOOK`.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `pnpm test -- src/api/interactiveTerminalClient.test.ts`
  - Expected: the new tests fail.

- [ ] **Step 3: Implement** the algorithm, prop and CSS.

- [ ] **Step 4: Run the tests.**
  - Run: `pnpm test -- src/api/interactiveTerminalClient.test.ts src/features/agents && pnpm typecheck && pnpm test:e2e -- tests/agents-terminal-controls.spec.ts`
  - Expected: all pass. Existing behaviour is unchanged without `autoControl`.

- [ ] **Step 5: Commit.**

```bash
git add src/api/interactiveTerminalClient.ts src/api/interactiveTerminalClient.test.ts src/features/agents/InteractiveTerminal.tsx src/features/agents/InteractiveTerminal.module.css
git commit -m "feat(terminal): optional auto control on open and takeover notices (spec 0007 §5.4)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task D5: Swarm tree and cards

**Files:**
- Create: `src/features/agents/swarm/SwarmTree.tsx`, `SwarmCard.tsx`, `Swarm.module.css`; `src/test/reactFlow.ts`
- Test: `src/features/agents/swarm/SwarmPage.test.tsx` (tree tests)

**Interfaces:**
- Consumes: `buildSwarmGraph`, `layoutSwarm`, `Selection` and `selectionId` (D2).
- Produces:
  - `SwarmTree({ view, ready, selected, onSelect }: { view: SwarmView; ready: number | null; selected: Selection | null; onSelect(s: Selection): void })`
  - `SwarmCard` (React Flow custom node type `"card"`)
  - `stubReactFlowDom(): () => void` (installs `DOMMatrixReadOnly` and `HTMLElement` `offsetWidth`/`offsetHeight` getters returning 100 when missing; returns a restore function)

`SwarmTree`:
- `<section aria-label="Swarm tree" className={styles.flow}>`
- `<ReactFlow nodes edges nodeTypes={{ card: SwarmCard }} fitView nodesDraggable={false} nodesConnectable={false} elementsSelectable proOptions={{ hideAttribution: true }} onNodeClick={(_, n) => onSelect(n.data.selection)} minZoom={0.25} maxZoom={2}>` containing `<Background gap={18} size={1} />` and `<Controls showInteractive={false} />`.
- Memoize `buildSwarmGraph` + `layoutSwarm` on `view` and `ready`. Never re-key `ReactFlow`, so pan and zoom survive polls.
- Mark the selected node via `node.selected = selectionId(n.data.selection) === selectionId(selected)`.
- Import `@xyflow/react/dist/style.css` here.
- Below the flow, render the legend `<ul className={styles.legend} aria-label="Legend">`: fast, standard, deep, unclassified, "Codex (dark ring)", "free slot", "waiting for a slot".

`SwarmCard`: a `<button type="button" className={…} data-kind data-state data-machine-kind aria-label={data.ariaLabel}>` with:
- a hidden target `Handle` (`Position.Top`) for every kind except server;
- `.title` and `.sub`;
- `.reason` when present;
- the meters grid;
- a hidden source `Handle` (`Position.Bottom`) for every kind except agent, free and waitingAgent.

`ariaLabel` = `` `${label}${sub ? `, ${sub}` : ""}${reason ? `, ${reason}` : ""}` ``.

`Swarm.module.css` (values from the mockup; tokens for surfaces and text):

```css
.flow { height: 540px; border: 1px solid var(--color-border); border-radius: var(--radius-panel); background: var(--color-canvas); }
.card { border: 1px solid var(--color-border); border-radius: 8px; background: var(--color-panel); color: var(--color-text); padding: 6px 10px; font: inherit; font-size: var(--font-size-xs); min-width: 120px; text-align: center; cursor: pointer; }
.title { font-weight: 700; } .sub { color: var(--color-text-secondary); }
.reason { color: #c7802e; font-weight: 600; }
.card[data-kind="server"] { border: 2px solid #57c; background: rgba(85,119,204,.12); }
.card[data-kind="machine"][data-machine-kind="local"] { border-left: 4px solid #6a8; }
.card[data-kind="machine"][data-machine-kind="cloud"] { border-left: 4px solid #59c; }
.card[data-state="offline"], .card[data-state="stopped"], .card[data-state="unknown"] { opacity: .55; border-style: dashed; }
.card[data-state="booting"] { border-style: dashed; border-color: #c7802e; }
.card[data-state="holding"] { border-color: #c7802e; }
.card[data-kind="node"] { background: var(--color-raised); }
.card[data-kind="waiting"] { background: var(--color-raised); border-style: dashed; border-color: #c7802e; }
.pill { min-width: 0; border-radius: 14px; padding: 3px 9px; color: #fff; border: none; }
.pill[data-class="fast"] { background: #3a9; } .pill[data-class="standard"] { background: #57c; }
.pill[data-class="deep"] { background: #a5c; } .pill[data-class="none"] { background: #6b7280; }
.pill[data-provider="codex"] { box-shadow: 0 0 0 2px #111 inset; }
.card[data-kind="free"] { background: transparent; color: var(--color-text-secondary); border: 1px dashed var(--color-text-secondary); }
.card[data-kind="waitingAgent"] { background: transparent; color: #c7802e; border: 1px dashed #c7802e; }
:global(.react-flow__node.selected) .card { outline: 2px solid #57c; outline-offset: 2px; }
.card:focus-visible { outline: 2px solid var(--color-focus); outline-offset: 2px; }
.meters { margin-top: 4px; display: grid; grid-template-columns: 36px 1fr 36px; gap: 2px 5px; align-items: center; color: var(--color-text-secondary); text-align: left; }
.meters[data-stale] { opacity: .6; }
.bar { height: 5px; border-radius: 3px; background: rgba(127,127,127,.2); overflow: hidden; }
.bar > i { display: block; height: 100%; } /* fill colour inline: >85 #d55, >65 #db4, else #3a9 */
.legend { display: flex; flex-wrap: wrap; gap: 14px; margin: var(--space-1) 0; padding: 0; list-style: none; color: var(--color-text-secondary); font-size: var(--font-size-xs); }
.legend i { display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 5px; vertical-align: -1px; }
```

- [ ] **Step 1: Write the failing tests** (`beforeAll(stubReactFlowDom)`, rendering `SwarmTree` with the adapted `SWARM_MOCKUP` inside the test `QueryClientProvider`/session helpers used elsewhere, or through `renderApp("/agents")` once D6 lands; for D5 render the component directly with a minimal wrapper):
  - **`renders every mockup state`:** buttons named `/^desktop, local · 24 cores/`, `/^laptop, local · offline/`, `/studio.*holding: Context measurement expired/`, `/^boat vm-a, cloud · booting/`, exactly three buttons named `"free"` (one under local-2, two under studio-1), and one matching `/^waiting/`.
  - **`offline machine renders dashed and greyed without meters`:** the laptop button has `data-state="offline"` and no `.meters` child.
  - **`codex pill has the provider ring`:** the button named `"codex · fast"` has `data-provider="codex"`.
  - **`clicking a card selects it`:** click `desktop` → `onSelect` called with `{ kind: "machine", machineId: "desktop" }`.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `pnpm test -- src/features/agents/swarm/SwarmPage.test.tsx`
  - Expected: the new tests fail.

- [ ] **Step 3: Implement** `SwarmTree`, `SwarmCard`, the CSS and the stub. In `SwarmCard`, use `.pill` plus `data-class={intelligenceClass ?? "none"}` and `data-provider={provider ?? undefined}` for agent and waitingAgent cards.

- [ ] **Step 4: Run the tests.**
  - Run: `pnpm test -- src/features/agents/swarm && pnpm typecheck`
  - Expected: pass.

- [ ] **Step 5: Commit.**

```bash
git add src/features/agents/swarm src/test/reactFlow.ts
git commit -m "feat(agents): swarm tree with React Flow cards for every swarm state

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task D6: Properties pane and page assembly

**Files:**
- Modify: `src/features/agents/swarm/SwarmPage.tsx` (complete)
- Create: `src/features/agents/swarm/SwarmPane.tsx`, `AgentPane.tsx`, `useAgentTokens.ts`, `SwarmPane.test.tsx`
- Modify: `src/features/agents/swarm/Swarm.module.css` (pane and stage rules)

**Interfaces:**
- Consumes:
  - `useRead` (D1 adapter; resource `"swarm"`, path `/api/v1/swarm`, `pollMs: SWARM_POLL_MS`, `enabled: cap.enabled`);
  - `useRead` overview (resource `"overview"`, path `/api/v1/overview`, scope `{ kind: "all_authorized" }`, `adapt: adaptOverview`, `pollMs: 10_000`) for `ready`;
  - `useProviderUsage()`, `useProjects()`;
  - `InteractiveTerminal` with `autoControl` (D4);
  - `adaptSessionDetail`, `adaptSessionUsage`, `sessionUsageJson`.
- Produces:
  - `SwarmPage()`
  - `SwarmPane({ view, selection, ready }: { view: SwarmView; selection: Selection | null; ready: number | null })`
  - `AgentPane({ agent }: { agent: AgentView })`
  - `useAgentTokens(sessionId: string): { text: string }`, which returns `"41k in / 6k out"`-style text (`k` = `Math.round(n/1000)` when ≥ 1000) or `"unknown"`

Page layout (`SwarmPage`):

```tsx
<AgentsTabs current="swarm" />
<div className={styles.stage} data-wide={selection?.kind === "agent" || undefined}>
  <SwarmTree view ready selected={selection} onSelect={setSelection} />
  <aside className={styles.pane} aria-label="Properties"><SwarmPane view selection ready /></aside>
</div>
```

- The selection is kept in the URL as `?select=<selectionId>`, so a reload keeps it. Use `useSearchParams` and preserve the other params.
- A selection id that no longer exists shows the empty pane.
- Coverage partial → a one-line note above the stage with `coverage.reason`.
- A read error → `QueryStatus` (existing component) inside the stage.

CSS:

```css
.stage { display: grid; grid-template-columns: minmax(0,1fr) 360px; gap: 10px; }
.stage[data-wide] { grid-template-columns: minmax(0,1fr) min(720px, 55%); }
@media (max-width: 1023px) { .stage, .stage[data-wide] { grid-template-columns: minmax(0,1fr); } .flow { height: 420px; } }
.pane { border: 1px solid var(--color-border); border-radius: var(--radius-panel); min-height: 540px; display: flex; flex-direction: column; overflow: hidden; background: var(--color-panel); }
.paneHead { padding: 10px 12px; border-bottom: 1px solid var(--color-border); background: var(--color-raised); }
.kicker { font-size: var(--font-size-xs); text-transform: uppercase; letter-spacing: .06em; font-weight: 600; color: var(--color-text-secondary); }
.paneTitle { font-weight: 700; font-size: var(--font-size-body); }
.facts { margin: 0; padding: 10px 12px; display: grid; grid-template-columns: 110px 1fr; gap: 5px 10px; font-size: var(--font-size-xs); }
.facts dt { color: var(--color-text-secondary); }
.actions { padding: 0 12px 10px; display: flex; gap: 6px; flex-wrap: wrap; }
.actionNote { padding: 0 12px 10px; margin: 0; color: var(--color-text-secondary); font-size: var(--font-size-xs); }
```

Pane content (exact labels; values formatted as shown; unknown → "unknown"; RAM GB = `bytes / 2**30` and disk GB = `bytes / 1e9`, both rounded to whole numbers):

| Selection | Kicker / title | Facts (dt → dd) | Actions |
| --- | --- | --- | --- |
| none | "Properties" / "Select anything in the tree" | Server → "health, version, queue depth"; Machine → "cores, memory, disk, load"; Worker node → "slots, heartbeat, backend"; Agent → "opens its live terminal" | — |
| server | "AQ server" / `location.host` | Health → `` `ok · v${version} · ${sourceRevision.slice(0,8)}` ``; Ready tasks → `` `${ready ?? "unknown"}` `` + (waiting branch shown ? `` ` (${waiting.agents.length || ready} waiting for a slot)` `` : ""); Running agents → `runningAgents`; Usage window → one `` `${providerId} ${usedPercent ?? "unknown"}%` `` per provider (preferred window), joined " · ", or "unavailable"; Projects → labels joined ", " or "unavailable" | link button "Open settings" → `/settings` |
| machine | "Local machine" or "Cloud machine (boat VM)" / label | State → online: "online"; holding: `` `holding: ${msgs}` ``; offline: `` `offline since ${local time of lastSeenAt}` `` or "offline"; stopped: "stopped"; booting: "booting"; unknown: "state unknown". Load now → `` `CPU ${c}% · RAM ${r}%` `` + (stale ? `` ` (as of ${formatAge})` `` : "") or "—"; Load avg → `` `CPU ${c}% · RAM ${r}% (last 15 min)` `` or "—"; Load source → "Windows host (WSL2)" / "Linux"; then, when hostProbe is stale or missing, a row Note → "Windows host probe stale: showing WSL VM load" / "Windows host probe missing: showing WSL VM load"; Max agents → `agentMax ?? "unknown"` + link button "Edit in Settings" → `/settings`; Cores / RAM → `` `${cpus} / ${gb} GB` ``; Disk free → `` `${gb} GB` `` or "unknown"; Worker nodes → `` `${nodes.length}` `` (+ `` ` (${offlineCount} offline)` `` when any); Cost (cloud only) → `` `$${(microsPerHour/1e6).toFixed(2)} / h · up ${formatAge(startedAt)}` `` or "—"; Holding (when any) → each `` `${message} — ${remedy}` `` on its own line | "Start worker", "Stop worker", "Drain" |
| node | "Worker node" / name | Backend → `backendKind ?? "unknown"`; Slots → `` `${occupied}/${total ?? "?"} slots` ``; Heartbeat → `` `${formatAge(lastSeenAt)} ago` `` or "never"; Service → running/stopped/draining or "not reported"; Version → `` `aq-worker ${workerVersion}` ``; Sandbox image → `` `${t.slice(0, 13)}…` `` for each template, or "none advertised"; Channel → "agent channel ready" / "agent channel unavailable" / "not reported"; Holding (when any) → as for machine | "Drain node", "Restart", "View logs" |
| free | "Free slot" / "waiting for ready work" | Node → node name; Takes → "any provider and class allowed on this node"; and when the machine is holding, Why → `` `Free, but this machine is holding back: ${holding[0].message}` `` and Fix → `remedy` | — |
| waiting | "Waiting" / `` `${n} waiting` `` | Why → each reason message; Fix → each remedy | — |
| waitingAgent | "Waiting agent" / title | Why → reasons joined "; "; Fix → remedies joined "; " | — |
| agent | `` `Agent · ${provider ?? "agent"} · ${class ?? "unclassified"}` `` / task title or "No task" | Task → `` `${project.label} / ${title}` ``; Attempt → `` `${attemptId.slice(0,8)} · ${formatAge(startedAt)}` ``; Model → `model ?? "unknown"`; Tokens → `useAgentTokens(sessionId).text` | "Cancel attempt" (disabled, title "Not available on this page yet"), link "Open task" → `/work/${task.id}`, link "Pop out terminal" → `/agents/${sessionId}` |

- **Agent terminal:** below the agent actions, render `<InteractiveTerminal key={terminalId} terminalId={terminalId} name={label} autoControl label="Agent terminal" />` when `terminalId` is set and `bootstrap.capability("terminal.watch").enabled`. Otherwise show `<p role="note">` with "No terminal was recorded for this agent." or the capability reason.
- **Worker controls:** the Start/Stop/Drain/Drain node/Restart/View logs buttons are `disabled` while `bootstrap.capability("swarm.worker.control")` is not enabled. Below them, `.actionNote` reads "Worker start, stop and drain are not available on this server yet."
- **Button style:** use `ActionButton` and the existing 32px pane-button style.
- **`useAgentTokens`:**
  1. `useRead` the session detail (resource `"session"`, path `` `/api/v1/sessions/${id}` ``, `adapt: adaptSessionDetail`);
  2. then `useQuery` with `queryKey(queryContext, "session-usage", { binding })`, `enabled: !!binding && capability("session.usage.read").enabled`, `queryFn: adaptSessionUsage(await sessionUsageJson(binding.attempt_id, signal), binding)`;
  3. text = `` `${fmt(input)} in / ${fmt(output)} out` `` when both counters are `known`, else "unknown".

- [ ] **Step 1: Write the failing tests** in `SwarmPane.test.tsx` (MSW default scenario plus `stubReactFlowDom`, through `renderApp("/agents")`):
  - **`server pane shows health version and running agents`:** click the "AQ server" card → `Health` dd matches `/^ok · v/`; `Running agents` dd is `"4"`.
  - **`machine pane shows load source and limits`:** click desktop → "Load source" is "Windows host (WSL2)"; "Cores / RAM" is "24 / 64 GB"; the "Start worker" button is disabled; the note text is present.
  - **`machine pane names a stale Windows probe`:** an override fixture with `host_probe: "stale"` → the Note row reads "Windows host probe stale: showing WSL VM load".
  - **`holding machine lists reason and remedy`:** studio → "Context measurement expired — Measure this worker's runtime again".
  - **`free slot on a holding machine explains why`.**
  - **`waiting agent pane gives why and fix`.**
  - **`agent pane opens a terminal with control requested`:**
    - setup: fixtures mode (`handlersFor("default", { fixtures: true })`) so the terminal handlers exist;
    - click "claude · standard";
    - expect a region named "Agent terminal", then `await screen.findByText("You have control")`. The mock always grants control, and `autoControl` takes it without a click.
  - **`agent pane is watch-only without terminal.control`:**
    - setup: bootstrap with `terminal.control` disabled;
    - expect "Watch only", and assert no `control` mutation was posted (spy on the MSW `/mutations` handler for `kind === "control"`).
  - **`selection survives reload through ?select=`:** `renderApp("/agents?select=machine:desktop")` → the machine pane renders.
  - **`pane widens for an agent`:** after an agent click, the stage has `data-wide`.

- [ ] **Step 2: Run them and confirm they fail.**
  - Run: `pnpm test -- src/features/agents/swarm/SwarmPane.test.tsx`
  - Expected: they fail.

- [ ] **Step 3: Implement** `SwarmPage`, `SwarmPane`, `AgentPane`, `useAgentTokens` and the CSS.

- [ ] **Step 4: Run the tests and build.**
  - Run: `pnpm test && pnpm typecheck && pnpm build`
  - Expected: all pass. `check-bundle` passes because React Flow lives in the lazy `agents/swarm` chunk.

- [ ] **Step 5: Commit.**

```bash
git add src/features/agents/swarm
git commit -m "feat(agents): swarm properties pane for every object; agent pane opens its terminal with control

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

### Task D7: E2E, visual reference and acceptance records

**Files:**
- Create: `tests/swarm.spec.ts`
- Create: `docs/design/swarm-tree-v3.html` (byte copy of the mockup) and `docs/design/14-swarm-tree-v3.png`
- Modify: `docs/design/manifest.json` (new asset entry), `docs/design/visual-acceptance.md` (table row "Agents › Swarm")

- [ ] **Step 1: Write `tests/swarm.spec.ts`.** Use the `desktop-1440` and `phone-390` projects; reuse the `axe` and `noOverflow` helpers from `tests/agents-terminal-controls.spec.ts`. Tests:
  - **`swarm landing renders every state`:** goto `/agents` → visible buttons for desktop, laptop (offline), studio (holding), boat vm-a (booting), a free pill and waiting; the legend is visible.
  - **`selecting an agent opens the terminal with control`:** click "claude · standard" → `getByRole("region", { name: "Agent terminal" })` is visible, and the status reads "You have control" (fixture terminal).
  - **`pan and zoom controls exist`:** `.react-flow__controls` is visible.
  - **`phone stacks tree above pane without horizontal scroll`:** run `noOverflow` at 390px and 320px.
  - **`axe has no violations on /agents`.**

- [ ] **Step 2: Run them.**
  - Run: `pnpm test:e2e -- tests/swarm.spec.ts`
  - Expected: pass on both projects.

- [ ] **Step 3: Add the reference.**
  - Copy the mockup to `docs/design/swarm-tree-v3.html`.
  - Render it: `pnpm exec playwright screenshot --viewport-size "1440, 900" --wait-for-timeout 3000 "file://$PWD/docs/design/swarm-tree-v3.html" docs/design/14-swarm-tree-v3.png`. It needs network access for esm.sh.
  - Add the manifest entry: `{"file":"14-swarm-tree-v3.png","title":"Agents swarm tree","source":".superpowers/brainstorm/2382938-1794083621/content/swarm-tree-v3.html","sha256":"<sha256sum>","bytes":<size>}`.
  - Add a visual-acceptance row:

    | Agents › Swarm | `14-swarm-tree-v3.png` | Top-down tree with the server at the root; machine cards with CPU/RAM bars; dashed free slots, greyed dashed offline, amber holding and booting, amber waiting branch; properties pane on the right that opens the agent terminal; controls bar deferred to phase 3 |

- [ ] **Step 4: Run the full suite.**
  - Run: `pnpm typecheck && pnpm test && pnpm build && pnpm test:e2e`
  - Expected: all pass.

- [ ] **Step 5: Commit and open the PR.**

```bash
git add tests/swarm.spec.ts docs/design
git commit -m "test(agents): swarm e2e at 1440 and 390; swarm mockup as a design reference

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin swarm-p2/dashboard
gh pr create --title "Swarm phase 2: Agents › Swarm page" --body "Spec aq-server 0007 phase 2 (dashboard). Pins aq-server swarm-p2 dashboard-read; merges right after the server PR, re-pinned to its merge commit (W76). Controls bar is phase 3.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

---

## Merge and acceptance

### Task M1: Merge the server and dashboard together, then the worker

- [ ] **Step 1: Confirm both PRs are green.**
  - Run: `gh pr checks <server PR> -R ElectricJack/aq-server` and `gh pr checks <dashboard PR> -R ElectricJack/aq-dashboard`
  - Expected: all checks pass.
- [ ] **Step 2: Merge the server PR** (`gh pr merge <n> -R ElectricJack/aq-server --merge`) and record the merge commit: `git -C aq-server fetch && git -C aq-server rev-parse origin/main`.
- [ ] **Step 3: Confirm nothing else changed the read contract.** Run `git -C aq-server log --oneline <S head>..origin/main -- protocol/dashboard-read`. Expected: only the merge commit.
- [ ] **Step 4: Re-pin and merge the dashboard.**
  - In the dashboard branch, run `python3 scripts/pin-contract.py ../aq-server dashboard-read <merge sha>` and `git diff --stat`. Expected: only the `commit`/`contract_commits` fields change.
  - Commit `contracts: re-pin dashboard-read to aq-server <short sha>`, push, wait for green, then `gh pr merge <n> -R ElectricJack/aq-dashboard --merge`.
- [ ] **Step 5: Re-pin and merge the worker.** In the worker branch, set `protocol/pinned/worker-machine/source.json` `commit` to the merge sha (the digests are unchanged), push, wait for green, then merge.

### Task A1: Live acceptance on aq-ms1 (stronger model)

Use the agent-browser skill with your own sessions, `--session qa-swarm-a` and `--session qa-swarm-b`. Follow `/home/jkern/dev/aq-platform/qa/2026-11-06/RULES.md`:
- never mutate the MS1 evidence tasks;
- **do not start agent runs without the coordinator's explicit go-ahead.**

Write `qa/2026-11-07/swarm-p2/plan.md` before you start and `results.md` (PASS/FAIL/BLOCKED with screenshots) at the end.

- [ ] **Step 1: Deploy the merged mains.**
  1. Move `wt/ms1h-server` and `wt/ms1h-worker` to their `origin/main`.
  2. Run `~/.local/share/aq-ms1/up.sh rebuild-server`.
  3. Run `~/.local/share/aq-ms1/rebuild-worker.sh`.
  4. In `aq-dashboard`, run `git merge --ff-only origin/main && pnpm install --frozen-lockfile && pnpm build`, then `~/.local/share/aq-ms1/up.sh server-up`.
  5. Restart the node controller with `up.sh node-stop && up.sh node`. This writes `machine.json`.
  6. Install the probe: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w <aq-worker>/packaging/windows/aq-host-load.ps1)" -Install -StartNow`.

  Expected:
  - `curl -s http://127.0.0.1:28080/healthz` is OK;
  - `/mnt/c/ProgramData/agent-q/host-load.json` is less than 10 s old;
  - after 30 s, `psql.sh -c "select node_id, received_at_ms from node_machine_reports"` shows a row.
- [ ] **Step 2: Live page at 1440×900 and 390×844.** Open `http://127.0.0.1:28080/agents` and check, against `docs/design/14-swarm-tree-v3.png`:
  - the tabs (Swarm current);
  - the server root card with its host;
  - machine "desktop" with CPU/RAM bars that move between polls. Cross-check against Windows Task Manager: start a CPU load on Windows (`powershell.exe -c "1..4 | % { Start-Job { while(1){} } }"`, stop it with `Get-Job | Stop-Job`) and see the CPU bar rise within 30 s;
  - the node card with backend and slots;
  - any offline nodes greyed and dashed, with their age;
  - holding reasons if any (for example an expired measurement);
  - the waiting branch only when ready work exists;
  - the server, machine and node panes against the D6 table (no invented values; unknown shows "unknown").

  Record which states the live data could not produce.
- [ ] **Step 3: Agent terminal.**
  - If an agent is running, click it: the terminal must connect with "You have control" without a click.
  - Open the same agent in `qa-swarm-b`: B shows "You took control from another viewer.", and A shows "Another viewer took control. You are watching." within one inspect cycle.
  - "Pop out terminal" opens `/agents/<session>`.
  - If no agent is running, ask the coordinator before `up.sh run`. If permission is not given, mark it BLOCKED.
  - Watch-only: if the stack cannot sign in a non-owner viewer, mark it "covered by `SwarmPane.test.tsx` only".
- [ ] **Step 4: Fixture states.**
  - Run `pnpm dev:mock` in aq-dashboard and open `http://127.0.0.1:5173/agents`.
  - Check every remaining mockup state: dashed free pills, laptop greyed and dashed with no meters, amber holding studio with its reason line, dashed amber booting VM, amber waiting branch with dashed pills, the Codex dark ring, and the legend.
  - Check each pane kind: server, machine, node, agent, free slot, waiting agent.
  - Capture 1440 and 390 screenshots.
- [ ] **Step 5: Report** reference / delivered visual subset / deferred differences (controls bar; per-agent waiting pills on live data; booting and cost on live data) / desktop and phone captures / served bundle digest (`curl -s http://127.0.0.1:28080/api/v1/bootstrap | jq .data.product.dashboard_digest`).

---

## Self-review notes

- **Spec coverage, §5.2 tree:** server root; machines with name, kind, CPU/RAM and agent maximum; worker node with backend and slots; agent pills by class and provider ring; free, offline, stopped, booting, holding and waiting states; pan and zoom. These are covered by S4, D2 and D5.
- **Spec coverage, §5.4 panes:** covered by D6. Phase 1 and 3 parts render disabled or "unknown", never invented.
- **Spec coverage, §6 WSL2 Windows load:** covered by W1, W3 and S3.
- **Spec coverage, §8:** measurement expiry is surfaced by S4 and S5 as `measurement_expired`; contracts merge together in M1.
- **§10 phase 2 row:** fully covered. The controls bar is deliberately omitted (Decision 1).
- **Types:** `Selection`, `SwarmView` and `HoldReasonView` are defined in D1 and D2 and used unchanged in D5 and D6. `aq_swarm::service::states` and `ServiceFact` are defined in S3 and consumed in S5.
