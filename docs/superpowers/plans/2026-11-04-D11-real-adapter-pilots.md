# D11 Real-Adapter Pilots Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (native; this is a single pool worker) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One integration test, `real_adapter_pilots_complete`, runs the routing, watcher, report and D9 coding pilots end to end through production AQ adapters on disposable PostgreSQL. Its output is the GATE-05 evidence in `docs/implementation/D11.md`.

**Architecture:**
- Each pilot is a sealed `Artifact` built from the pinned profile registry, using the same builder style as D9's `coding.rs`.
- Each pilot is installed and activated in a production `Driver` over a production `Service`. A run is admitted with `Service::prepare` and `Driver::admission`, then driven with `Driver::quantum`.
- Host effects go through `MailHost` → `Service::host_dispatch` (the real adapters).
- Fakes stand in only behind injected ports.
- Fault cases reuse the existing `test-support` hooks and the adapter discipline: the intent row commits, then the effect runs, then the finish seals the result.

**Tech Stack:** Rust, tokio, sqlx/PostgreSQL (`scripts/test-db`), `aq_playbook_host` (vendored aq-playbooks 0173673; see Task 0), host catalog 2.0.0.

**Spec:** task `crisp-meadow-36.12` description; `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml` (D11 ≈ line 2574, D10 ≈ 2514, D4 P19 ≈ 2137).

## Global Constraints

- **Fakes are limited to** a deterministic model provider, a fake notification destination, and a local bare Git remote. Any other fake is a recorded limit in D11.md, never silent.
- **Exactly twelve production commands registered** (D4 P19). Today Host7 registers 9 (`host_adapters.rs:100`). See Decision gate.
- No real sleeps or long timeouts. Deadlines are driven by the service clock (`f.state.clock`) and are never waited out in real time.
- Run with `RUST_TEST_THREADS=2 CARGO_BUILD_JOBS=2`, with the DB set up by `eval "$(scripts/test-db)"`.
- Don't edit `docs/architecture/*` or `docs/specs/*`.

## Decision gate (escalated 2026-11-04, msg-95f25db2922740b6bf4859a87d84ecef)

**Resolved 2026-11-04 by the operator:** finish D11 to its acceptance (option A for the
three adapters) with tight scope. Task 0 (the D10 re-vendor) is deferred as a follow-up,
and PB-04 is not claimed. The outcome is in `docs/implementation/D11.md`.

The following are outside D11's stated scope and await the supervisor's decision:

- Host7 catalog adapters for `message.send` and `decision.request`.
- `findings.reconcile` on Host7.
- Re-vendoring D10.

Tasks 1–4 are needed under every option. Task 5's last step and Task 0 depend on the decision:

- **A:** implement Task 0, Task 5b and Task 6b here.
- **B:** stop after Tasks 1–4 and wait for follow-ups F-a/F-b.
- **C:** Task 5 ends with `message.send` refused, and D11.md lists the gaps as remaining.

## Gaps found while planning (recorded as limits, not fixed here)

- **Graph:** aq-graph is a separate service, so the graph is an in-memory `WorkGraph` fake (the D5–D9 pattern). This is outside the three allowed fakes.
- **Check executor:** production leaves the executor unbound (D6 "Not in D6", D7 l.147). The coding pilot's `check.request` needs a fake `CheckExecutor`, which is also outside the allowed fakes.
- **Health observations:** `context.read view=health_observations` exists only under Monitoring6 (`host_adapters.rs` context_read). So the watcher pilot runs on the production `monitoring_v6` profile, in its own namespace, inside the same test.

## Review Focus

1. **A stale baseline** (snapshot from an earlier scan revision) must never clear an episode. Expected outcome: `conflict`, with the episode left open.
2. **A missing page** (`page != 0`, or an `incomplete` health read) must never reach `findings.reconcile`.
3. **A replayed `work.change`** after a concurrent graph bump must return `conflict`, never `changed`.
4. **A late `supervisor_authoring` completion** after the report deadline must be retained, and no second report may be sent.
5. **Revoking the grant** between two pilot steps must refuse the next effect; no effect row is created.

---

### Task 0 (decision A only): Re-vendor aq-playbooks to D10

**Files:**
- Create: `vendor/aq-playbooks-<rev>/` (a copy of the aq-playbooks crates at the chosen revision ≥ c0d0376).
- Create: `vendor/aq-playbooks-source-<ver>.json`.
- Modify: workspace `Cargo.toml` and `crates/aq-server/Cargo.toml` (path deps), and `Cargo.lock`.

- [ ] Copy the crates. Record `source_revision`, `vendor_root` and the tree digest, following the format of `vendor/aq-playbooks-source-0.1.3.json`.
- [ ] Run `cargo build --locked -p aq-server`, then the D8 and D9 regression: `cargo test --locked -p aq-server --test playbook_findings_triggers -- --test-threads=1` and `cargo test --locked -p aq-server --lib coding`. All must pass.
- [ ] Commit `build: vendor aq-playbooks <rev> (D10)`.

### Task 1: Pilot harness and the D9 coding pilot through the real Host7 driver

**Files:**
- Create: `crates/aq-server/tests/real_adapter_pilots.rs`
- Create: `crates/aq-server/tests/common/pilots.rs` (harness: `Pilot` struct, fakes)

**Interfaces:**
- Produces: `Pilot::host7(f) -> Pilot` and `Pilot::monitoring6(f) -> Pilot`. These build the Service and Driver with the keyring, grant (all profile capabilities) and protected PostgreSQL storage (copy `protected_factory` from `playbook_findings_triggers.rs:773`).
- Produces: `Pilot::run(&self, artifacts: &[Artifact], binding: &str, input: Value) -> RunView`, which installs in order, activates the last, prepares, admits, and runs quanta until the run is terminal or 64 quanta pass. Here `RunView = Driver::inspect_run` JSON.
- Produces the fakes:
  - `FakeGraph` (copy `playbook_work_completion.rs:25`, plus `update_node` with an `expected_version` check returning 409).
  - `FakeModel: JudgmentProvider`, which returns deterministic assessments keyed by the request digest.
  - `FakeChecks: CheckExecutor`, which reports `exited 0` on the first `observe`.
  - `BareRemote`, a `git init --bare` tempdir used through the D7 `Integration` test adapter.

- [ ] Write `real_adapter_pilots_complete`. For the coding part, call `CodingPolicy::project_default(vec![unit])`, run `pilot.run(&artifacts.install_order(), "coding", policy.input(&req))`, and assert:
  - the run outcome is `published`;
  - there is exactly one `candidate.publish` row in `playbook_effects`;
  - the bare remote's destination ref equals the prepared candidate OID.
- [ ] Run the test. Expected: FAIL (harness missing).
- [ ] Implement the harness. Drive async work completions with `driver.test_collect_work_completions()` between quanta, and set the graph task to `completed` with `aq:review_disposition=accepted`, the way the task lifecycle would.
- [ ] Run the test. Expected: PASS.
- [ ] Commit.

### Task 2: Routing pilot (Host7)

**Files:**
- Create: `crates/aq-server/src/playbooks/pilots.rs` (a `pub fn routing() -> Result<Artifact, CodingError>`; reuse `coding.rs` helpers by making `lit/r/fields/eq/edges/decide/ret/call` `pub(super)`).
- Modify: `crates/aq-server/src/playbooks/mod.rs` (`pub mod pilots;`).
- Test: the routing section of `real_adapter_pilots_complete`.

**The flow:**
1. `context.read{view:"execution_options", subject:{id:<attempt>, revision:0}, page:0, snapshot:""}`.
2. If `complete` is false, go to `judgment.request` (fake model), then continue.
3. `context.read{view:"work"}` to get the current task revision.
4. `work.change{variant:"execution_constraints", subject:{id:task, revision:<read revision>}, value:<chosen option>, hold_id:null}`.
5. Return `routed{reference, subject}` on `changed` and `stale` on `conflict`.

- [ ] Add assertions:
  - With seeded `intelligence_attempt_selections` from `common::profiled::insert`, the run is `routed` and makes 0 `judgment.request` dispatches.
  - With an unknown attempt id, the run is `routed` and makes exactly 1 `judgment.request` dispatch.
  - After a `FakeGraph::set` bump between the read and the change, the run is `stale` and the node metadata is unchanged (Review Focus 3).
- [ ] Run. Expected: FAIL (`pilots::routing` missing). Implement. Run. Expected: PASS. Commit.

### Task 3: Watcher pilot (Monitoring6)

**Files:**
- Modify: `crates/aq-server/src/playbooks/pilots.rs` (`pub fn watcher(reminder_ms: u64) -> Result<Artifact, CodingError>`).
- Test: the watcher section.

**The flow:**
1. `driver.start_health_scan`.
2. `context.read{view:"health_observations", page:0}`. Anything other than `observed && complete` returns `incomplete` without calling reconcile.
3. `findings.reconcile{observation:<reference>, partition, evaluation, findings[{key, evidence, expected_revision:<prior episode revision>}], reminder_ms:<param>}`.

- [ ] Assertions:
  - The first run is `reconciled` and opens an episode at revision 1.
  - A second run with a stale `expected_revision` is `conflict`, and the episode stays open (Review Focus 1).
  - A run whose read answers `incomplete` makes 0 `findings.reconcile` dispatches (Review Focus 2).
  - `reminder_ms` is the value passed in (parameterized: 60_000 and 300_000).
  - Fake notification destination: the notifications owner's `enqueue` rows are counted once per episode transition.
- [ ] FAIL → implement → PASS → commit.

### Task 4: GATE-05 fault cases

**Test:** the faults section, against the Host7 pilot from Task 2 and the coding pilot from Task 1.

- [ ] **Crash between the domain effect and the runner receipt.** `driver.stop()` after the effect row reaches `finished` but before the next quantum. Then `Driver::start` again, `recover()`, and quanta. Assert that exactly one graph mutation is made (FakeGraph replies counted by idempotency key).
- [ ] **Lost effect response.** `driver.test_lose_next_decision_reply()` covers decisions. For host commands, use a `FakeGraph` that drops the first reply after applying it. Assert the run completes through `reconcile` (`host_retained`) and the second mutation replays the same key.
- [ ] **Revoked authority.** Revoke the grant through `Service` between quanta. Assert the next effect is refused and no new `playbook_effects` row appears (Review Focus 5).
- [ ] **Changed candidate or base revision.** Advance the bare remote's base between prepare and publish. Assert publish is refused or `conflict`, and the destination ref is unchanged.
- [ ] **Notification retry.** The fake destination fails the first delivery. Assert one delivery after retry and one notification row.
- [ ] FAIL → (fix only in tests; adapter bugs become direct tasks against D5–D8) → PASS → commit.

### Task 5: Report pilot (Host7)

**Files:**
- Modify: `crates/aq-server/src/playbooks/pilots.rs` (`pub fn report() -> Result<Artifact, CodingError>`).

**The flow:**
1. `context.read{view:"project"}` freezes the visibility-scoped window. Its `reference` and `snapshot` are bound once.
2. `work.ensure{kind:"supervisor_authoring", lifetime:"owned", logical_key:"<report key>", visibility:"project"}`. Wait for its completion up to the deadline.
3. On completion before the deadline, use the authored reference. On the deadline, use a deterministic fallback body (window reference and snapshot).
4. `message.send` exactly once.

- [ ] **5a (all options).** Assert:
  - exactly one `work.ensure` dispatch with kind `supervisor_authoring`;
  - deadline path: advance the service clock past the deadline, and the fallback is chosen;
  - a late completion, collected after the fallback, is retained in `late_results` and makes no second `message.send` (Review Focus 4).
- [ ] **5b (A: implement the Host7 `message.send` adapter first; C: the run ends `refused` at `message.send`).** Under A, assert one queued message at the fake notification destination.
- [ ] FAIL → implement → PASS → commit.

### Task 6: Evidence and registration check

- [ ] **6a.** In the test, record versions in a JSON written to `target/d11-evidence.json`:
  - the `aq-server` crate version;
  - the `aq_playbook_host` vendored revision;
  - the catalog `2.0.0` fingerprint `sha256:e2ce7dc8…43db`;
  - each pilot artifact `digest`.
- [ ] **6b.** Assert `HOST7_COMMANDS.len()` against the registry. Under A, assert `== 12` and that the set equals the registry's `commands` keys. Under C, the test pins `9` and D11.md lists the 3 missing commands as remaining.
- [ ] Write `docs/implementation/D11.md`. It must cover:
  - versions and digests;
  - the command count;
  - the fakes used, with the two extra fakes as limits;
  - each pilot's assertions;
  - the fault cases;
  - remaining GATE-05 items: H3, H4, H6, F6, plus any decision-gated gaps.
- [ ] Run the deliverable `cargo test --locked --workspace real_adapter_pilots_complete`. Expected: 1 passed.
- [ ] Squash, `git merge origin/main`, `aq git push`, close.
