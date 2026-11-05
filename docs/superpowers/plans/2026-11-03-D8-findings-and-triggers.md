# D8 Findings and Triggers Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task-by-task in the assigned worktree. The task authorizes continuous execution in this session.

**Goal:** Reconcile durable finding episodes and admit playbooks from confirmed events and restart-safe zoned daily schedules.

**Architecture:** Add an explicit catalog profile 6, preserving the existing profiles and pinned host contracts. AQ transactions own immutable health scan baselines, episode revisions, notification intents, schedule cursors and sealed outgoing events. Existing engine event receipts own destination deduplication and catalog revisions; existing notification enqueue owns logical messages and delivery.

**Tech Stack:** Rust, Tokio, SQLx/PostgreSQL, pinned aq-playbooks runner and D4 host registry.

**Spec:** Task crisp-meadow-36.9, contract D8 in `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml`; findings semantics in aq-playbooks spec §6.

## Global Constraints

- Build independently with the committed lockfile and `--locked`; no sibling dependencies.
- Preserve graph lifecycle authority and existing pinned contract bytes/profiles.
- Use the assigned branch and guarded AQ publication; the integration owner delivers to main.
- No notification transport/policy, activation API, stall detection or pilot playbooks.
- Named test filters must execute real passing tests. Use foreground Cargo with the slot target, two build jobs and two test threads (`docs/guides/cargo-worker-builds.md`).
- Follow the owner's focused-test velocity rule; record exact results and limits in `docs/implementation/D8.md`.

## Review Focus

1. Reused event identity with different bytes conflicts; no subscription retains its catalog revision.
2. A crash between engine commit and AQ acknowledgement does not create another root run.
3. DST gaps/overlaps and clock rollback preserve local-date occurrence identity.
4. An old complete scan cannot clear a concurrently updated episode, even with a new supplied revision.
5. Capacity refusal rolls back schedule cursors/episodes; notification retries preserve logical identity.

### Task 1: Durable trigger admission and daily calendars

**Files:** Create `crates/aq-server/src/playbooks/triggers.rs` and `crates/aq-server/tests/playbook_findings_triggers.rs`; modify `review_domain.rs`, `service.rs`, `driver.rs`, `runtime.rs`, `config.rs` under the same playbooks directory; add the next free migration in `crates/aq-server/migrations/`.

**Interfaces:** Consume `Request::Event`, `Fanout`, `EventStatus`, D3 confirmed settlements and `Service::budget`. Produce `CatalogProfile::Monitoring6`, `Service::new_monitoring6`, `Service::put_schedule(owner, definition, expected_revision)`, `Service::collect_schedule_events(owner, limit)` and existing event receipts.

- [x] Write `domain_event_trigger_admits_once`: install/activate a real event-triggered artifact, send a confirmed event twice across owner restart, assert one root, original destinations and content conflict; assert no-match receipt retains catalog revision.
- [x] Write `schedule_survives_restart_without_double_fire`: persist a daily schedule, collect a due occurrence, reconstruct the owner, collect again and assert one sealed occurrence; include revision CAS and backpressure rollback.
- [x] Run the new test target and observe missing adapter/profile failures.
- [x] Add profile 6 and durable schedule registry. PostgreSQL owns IANA timezone interpretation: gaps shift forward and overlaps select standard time. Date + schedule revision is the stable occurrence identity. Skip discards old occurrences, coalesce emits the latest due date, bounded catch-up advances only through the emitted bounded batch.
- [x] Collect schedule occurrences atomically with cursor changes before engine delivery. Enable confirmed settlement collection for profile 6 and retain existing keyed manual admission.
- [x] Run both named tests and focused bridge regression tests, then commit.

### Task 2: Findings and immutable observation coverage

**Files:** Create `crates/aq-server/src/playbooks/findings.rs`; modify `host_adapters.rs`, the new migration, and `playbook_findings_triggers.rs`.

**Interfaces:** Consume profile-6 grants, the pinned `findings.reconcile` contract, and notifications `Service::enqueue`. Produce trusted-host scan baseline/completion APIs, reconciliation dispatch, bounded notification draining, and a health view exposing fixed episode revisions through the existing snapshot/reference fields (no pinned wire schema changes).

- [x] Write tests for present/unchanged/reminder/clear, immutable observation replay, partial scans, forged completeness and stale clears; assert atomic notification intent creation and enqueue deduplication.
- [x] Watch tests fail before implementing.
- [x] Capture episode revisions before scans; accept host completion only for that immutable partition/observation/evaluation identity. Positive findings may use incomplete coverage. Missing keys clear only with verified complete coverage and the original baseline revision; conflict rolls back the entire reconciliation.
- [x] Persist observation-level replay receipts and notification requests in the episode transaction. Drain through notifications enqueue after commit; retry uses the frozen request and IDs.
- [x] Register dispatch/recovery and health snapshot projection without changing old profiles.
- [x] Run focused tests and commit.

### Task 3: Verification and delivery

**Files:** Create `docs/implementation/D8.md`.

- [x] Run the two exact named workspace commands, the new integration test target, affected bridge/catalog tests, formatting and targeted clippy. Record failures or unavailable checks accurately.
- [x] Review the final diff against every D8 requirement; resolve correctness findings. This task is not marked `review: adversarial`.
- [x] Write evidence with commands, environment, source base, implementation commit and remaining limits.
- [x] Squash unpublished feature history preserving the final tree and attribution. Fetch origin, merge origin/main, choose the next free migration number and update its meta version.
- [x] Run targeted checks for any merge changes; guarded push, save the full SHA, settle subtasks and close with changes/verification/test evidence and claim-next.
