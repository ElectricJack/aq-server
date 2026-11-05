# H3 Stall Ladder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** AQ detects stalled sessions from evidence and climbs a bounded, persisted ladder: observe, nudge at a safe boundary, notify, stop and preserve, replace, quarantine and escalate. It never mistakes waits, human control or missing data for a stall.

**Architecture:** A pure deterministic `ladder::step(episode, evidence, policy, now)` returns the next episode state plus at most one action intent. `engine::tick` persists the intent (stable action id, expected revision) before it runs the effect through a `Ports` trait. It then records an effect receipt. Ports are idempotent by action id, so a restart re-runs intents that have no receipt and never repeats an effect. Production ports read observation, terminal and B3/B4 state, and write through the F3 mailbox, F4 notifications and B3/B4.

**Tech Stack:** Rust, axum, sqlx/Postgres, serde, schemars, async_trait.

**Spec:** task `clear-crest-61.3` description, `docs/architecture/0004-*.md` §2.3 (stall ladder), `docs/architecture/0003-*.md` §5.1 (Unknown input blocks input).

## Global Constraints

- Acceptance: `cargo test --locked --workspace stalled_session_is_nudged_then_escalated` and `cargo test --locked --workspace nudge_never_injects_during_unknown_input_state` must exist and pass.
- S5: heartbeats, repaints, transcript mtime, AQ-injected text and token growth alone are not progress.
- Unknown observations raise a monitoring incident. They never trigger destructive recovery (stop, replace or quarantine).
- While input state is unknown or a human controller holds the pane, a nudge is deferred to the inbox and never injected.
- Notify once per episode. Quarantine is scoped to session/lineage, never the whole worker.
- Progress opens a new evidence interval. It never resets lineage-wide ceilings.
- Migration: next free number after `origin/main` (currently 0077 after the 0075 duplicate fix). It sets `state_format_version`.
- No real sleeps in tests. Time is injected (`now: u64` ms).

## Review Focus

- A human controller holding the pane combined with an eligible nudge that was already queued: poll-time filter in `node_poll` excludes it (test in Task 4).
- An observation stream that is all Usage/user text after a nudge does not reset the clock (test in Task 1).
- Daemon restart between intent and receipt does not repeat the effect (test in Task 3).
- A stop that is never confirmed quarantines rather than replaces (test in Task 1).
- A cancelled session suspends the ladder with no action (test in Task 1).

---

### Task 1: Policy, evidence and pure ladder

**Files:**
- Create: `crates/aq-server/src/stall/mod.rs`, `stall/policy.rs`, `stall/evidence.rs`, `stall/ladder.rs`
- Modify: `crates/aq-server/src/lib.rs` (`pub mod stall;`)

**Interfaces (produces):**
- `policy::ExecutionPolicy { version, evidence_window_ms, grace_ms, nudge_limit, nudge_backoff_ms, max_deferrals, stop_confirm_ms, replacement_ceiling, long_check_ms, escalation: String }`, `ExecutionPolicy::v1()`, `validate() -> Result<(), &'static str>`.
- `evidence::Explanation { ProviderBackoff, RegisteredWait, KnownLongCheck, HumanControl, ObservationsUnavailable }`, `InputState { Ready, Unknown, HumanControlled }`, `Admission { cancelled, held, hibernated, terminal_owned }`.
- `evidence::Evidence { last_progress_ms: Option<u64>, observed_through_ms: Option<u64>, explanation: Option<Explanation>, input: InputState, admission: Admission, stop_confirmed: bool }`.
- `evidence::is_progress(&ObservationKind) -> bool`. Candidate progress: TurnBoundary, Assistant TextReference, ToolStarted, ToolResult, Question. Not progress: Usage, User TextReference, ProviderError, Gap.
- `ladder::Rung { Observing, Suspected, Nudging, Notified, Stopping, Replacing, Replaced, Quarantined }`, `ladder::Episode` (serde), `ladder::Intent { MonitoringIncident, Nudge{ordinal, defer}, Notify, Stop, Replace, Quarantine{reason} }`, `ladder::Outcome` (port result fed back), `ladder::step(&Episode, &Evidence, &ExecutionPolicy, now) -> (Episode, Option<Intent>)`, `ladder::apply(&mut Episode, &Intent, &Outcome, now)`.

- [ ] Write tests in `ladder.rs`:
  - `ladder_walks_every_rung_in_order`
  - `explanations_hold_the_ladder`
  - `unavailable_raises_incident_once_never_destroys`
  - `aq_nudge_text_does_not_reset_clock`
  - `progress_opens_interval_keeps_ceilings`
  - `unconfirmed_stop_quarantines`
  - `admission_recheck_suspends`
  - `deferrals_escalate_to_notify`
- [ ] Implement until they pass. Commit.

### Task 2: Migration, F4 kind and persistence

**Files:**
- Create: `crates/aq-server/migrations/0077_stall_ladder.sql`, `crates/aq-server/src/stall/store.rs`
- Modify: `notifications/policy.rs` (`Kind::SessionStalled` = `"session.stalled"`, ALL becomes 9), `notifications/sources.rs` (`observe_stall(source, scope, attempt, title)`)

Tables:
- `stall_episodes(episode_id PK, attempt_id UNIQUE, lineage_id, revision, state jsonb, updated_at_ms)`
- `stall_actions(action_id PK, episode_id, kind, expected_revision, intent jsonb, receipt jsonb NULL, delivery NULL CHECK attempted|deferred|delivered|unknown)`
- `stall_lineage_budgets(lineage_id PK, replacements)`
- `stall_nudges(message_id PK, attempt_id, action_id, inbox_only)`
- `stall_quarantines(attempt_id PK, lineage_id, task_id, reason, held jsonb)`
- The notification kind constraint now includes `session.stalled`.

- `store::open(pool, target, now) -> Episode`, `store::commit(pool, &Episode, expected_revision, Option<(Uuid, &Intent)>) -> Result<bool>` (CAS), `store::pending(pool, episode_id)`, `store::receipt(pool, action_id, &Outcome, delivery)`.
- `stall::action_id(episode_id, kind, ordinal) -> Uuid` (UUID v5).
- [ ] Update every test/listing that enumerates `Kind::ALL` (`tests/notification_policy.rs`). Commit.

### Task 3: Engine with ports; acceptance `stalled_session_is_nudged_then_escalated`

**Files:** Create `crates/aq-server/src/stall/engine.rs`, `stall/tests.rs`.

- `engine::Target { attempt_id, session_id, task_id, lineage_id, project_id, workspace_id, generation }`.
- `#[async_trait] engine::Ports { async fn evidence(&self, &Target, &ExecutionPolicy, now) -> Result<Evidence>; async fn effect(&self, &Target, action_id: Uuid, &Intent) -> Result<Outcome> }`.
- `engine::tick(pool, ports, target, policy, now) -> Result<Option<Intent>>`. It reconciles pending intents first, then steps, commits the intent, runs the effect and records the receipt.
- [ ] DB test `stalled_session_is_nudged_then_escalated` with a fake recording port. Time advances through every rung. The test asserts the effect order incident-free: nudge×limit, notify once, stop, replace. A second stall in the replacement's lineage with the ceiling reached quarantines. A restart (an intent persisted, effect not receipted) re-runs that effect exactly once with the same action id.

### Task 4: Production ports, F3 safe-boundary nudge; acceptance `nudge_never_injects_during_unknown_input_state`

**Files:** Create `stall/ports.rs`. Modify `agent_access/messaging.rs`:
- `system_send_in_tx`
- the `node_poll` filter (`NOT EXISTS stall_nudges inbox_only`, and no stall nudge while the attempt's input is not ready)
- `pub(crate) async fn injectable(tx, attempt, task_key, session_key, limit)`

- `ports::input_state(pool, attempt) -> InputState`
- `ports::nudge(state|tx, target, action_id, body, input) -> Outcome::Nudged{message_id, delivery}`
- `DbPorts` (evidence SQL, cancel via B3, replace via B4 `assess_task`+`release`, quarantine insert, notify via `observe_stall`)
- The B4 `assess_task` refuses a quarantined task (`stall_quarantined`).
- [ ] DB test `nudge_never_injects_during_unknown_input_state`:
  - A profiled attempt, launch binding and terminal row with a `delivery_unknown` input receipt.
  - The nudge is stored `inbox_only` and is readable via the inbox. `injectable` returns nothing.
  - An eligible nudge queued earlier is also withheld once a controller principal holds the pane.

### Task 5: Owner `session.nudge` action and runtime

**Files:**
- Create: `stall/action.rs` (ActionKind `session.nudge`, payload `{operation_id, project_id, expected_generation, body}`, same safe-boundary nudge path)
- Create: `stall/runtime.rs` (sweep loop over attempts holding tasks, spawned in `lib.rs`)
- Modify: `dashboard_api/actions/existing.rs` (register and schemas). Regenerate the action schema if a snapshot exists.

### Task 6: DoD and evidence

- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and the targeted tests.
- Write `docs/implementation/H3.md`.
