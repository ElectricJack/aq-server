# E0 Local Full Stack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run PostgreSQL, pinned aq-graph, supervised aq-git and aq-server locally, with observable dependency recovery and repeatable development seeding.

**Architecture:** Extend the existing development overlay without enabling the production tunnel. aq-git uses a private shared Unix socket and retained state; Compose supervises its process. Server readiness probes its configured Graph and Git peers with bounded reads. Bootstrap uses public component APIs, never writes graph lifecycle tables directly, and accepts only explicitly supplied scratch repository values.

**Tech Stack:** Rust 1.96.1, PostgreSQL 17, Docker Compose, Python 3 standard library, Bash.

**Spec:** `docs/architecture/0005-sandboxed-execution-and-deployment-topology.md` §§3,9; `docs/specs/0001-infrastructure-foundation.md`; E0 in `2026-11-02-v2-full-application-tasks.yaml`.

## Global Constraints

- Builds: each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs.
- Rust uses the committed lockfile with `--locked`.
- aq-graph owns task state; AQ owns grants, attempts and dispositions.
- No secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions.
- While v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- Only A-series and docs tasks edit canonical architecture/specification documents.
- Use focused/area tests under the AQ resource controls; full-suite validation belongs to CI, per worker prime.
- Evidence: `docs/implementation/E0.md`, exact commands, environment, revision and limits; publish through guarded AQ Git.
- Supervisor direction `msg-b57c2fa327114cd7b904aaa6867dd6b8`: do not guess the scratch GitHub repository. Seed reads `DEV_SCRATCH_REPO_URL`, `DEV_SCRATCH_BASE_BRANCH`, `DEV_SCRATCH_CREDENTIAL_REF` without defaults; missing values are explicit. Test with an owned local bare repository; an unanswered owner designation may remain documented at close.

## Review Focus

1. Git crashes while server remains alive: readiness degrades and automatically recovers after supervision restarts it.
2. A stale socket or malformed peer response must not imply readiness; probe responses are bounded and correlated.
3. Existing foundation configurations without Graph/Git retain their readiness wire shape; old reports remain readable.
4. Repeated bootstrap must preserve credentials and return the same project/task rather than duplicating them.
5. Missing or changed scratch inputs must be explicit, and testing must not touch an owner repository or existing Compose project.

## File Structure

- `crates/aq-server/src/readiness.rs`: configured peer probes and readiness aggregation.
- `crates/aq-release/src/spool.rs`: optional dependency status in the shared readiness report; adjust report construction sites and compatibility tests.
- `crates/aq-server/tests/readiness.rs`: missing peers, malformed replies, recovery and legacy behavior.
- `compose.dev.yaml`: graph/Git services, private socket/state mounts, readiness health checks.
- `scripts/dev-stack.py`, `scripts/dev-up`: secret/vault preparation and public API seed; no scratch default.
- `scripts/compose-smoke.sh`, `scripts/compose-smoke.py`: named disposable acceptance scenario, recovery and local bare repository seed.
- `scripts/test-dev-stack.py`: bootstrap input/replay tests.
- `docs/development.md`, `docs/implementation/E0.md`: commands, exact inputs, evidence and remaining limits.

### Task 1: Bounded dependency readiness

**Interfaces:** Consumes existing `Config.agent_access.graph_url` and `Config.source_retention.git_socket`. Produces optional `ReadinessReport.dependencies`, mapping `graph` and `aq_git` to `ready`/`degraded`; HTTP 503 if any configured peer is degraded. Unconfigured reports omit the map.

- [ ] Add readiness tests asserting configured unavailable peers give HTTP 503, valid peers give 200, malformed/mismatched Git responses refuse, and a lost/restarted socket recovers without restarting AQ.
- [ ] Run the readiness test target; expected new assertions fail because peers are not checked yet.
- [ ] Add bounded parallel Graph HTTP health and Git capabilities probes, preserving the loopback fence. Add default/omitted dependency map to the shared report and update its consumers' constructors.
- [ ] Run readiness plus aq-release spool compatibility tests; expected all selected tests pass, including legacy JSON decode and absence of dependencies for foundation-only configurations.
- [ ] Commit `feat: report configured graph and git readiness`.

### Task 2: Development assembly and seed

**Interfaces:** `scripts/dev-stack.py prepare` creates private generated vault/secrets/state; `seed` reads explicit scratch environment values and returns public project/task identifiers. `scripts/dev-up` prepares then starts the existing Compose overlay. Graph uses a full pinned revision as its build context; aq-git builds the existing pinned source and runs under Compose restart supervision.

- [ ] Add bootstrap tests for all-unset inputs, partial inputs, credential-bearing URLs, replay and local bare repository seed. Add Compose contract assertions to the smoke scenario before implementation.
- [ ] Run the tests and scenario; expected missing implementation/services fail.
- [ ] Implement generated configuration and idempotent preparation, Graph bootstrap credential, separate Graph database, private Git socket and storage, retained source staging, explicit seed inputs, public Graph task creation, public Git registration, and AQ profile/project binding APIs.
- [ ] Configure services to restart on process failure; no published Git socket, no Docker socket, only loopback dev listeners. Make generated vault/spool paths overrideable so smoke owns all its resources.
- [ ] Run the bootstrap tests and Compose configuration validation; expected all tests pass and the default tunnel remains disabled.
- [ ] Commit `feat: assemble and seed the local development stack`.

### Task 3: Disposable full-stack acceptance and documentation

**Interfaces:** `scripts/compose-smoke.sh dev` runs named test `compose_dev_reaches_ready_with_graph_and_git` and prints an explicit PASS only after every service is healthy and live recovery has been observed. Evidence includes exact image identities, readiness transitions, seed IDs and cleanup.

- [ ] Implement the scripted test against uniquely named containers/volumes and allocated loopback ports; use an owned local bare repository as scratch input.
- [ ] Assert server reports Graph/Git ready, the graph task and project binding exist, and seed replay leaves one of each. Kill aq-git, observe HTTP 503/degraded, then a different process incarnation and 200/ready. Also verify Graph outage degrades readiness.
- [ ] Run `scripts/compose-smoke.sh dev`; expected the named test passes with readiness/seed evidence and owned resource cleanup confirmed.
- [ ] Run `cargo fmt --check`, focused readiness/release/updater checks and strict Clippy for touched crates. Record unavailable checks or baseline failures truthfully; no full-suite expansion contrary to worker prime.
- [ ] Document WSL2 setup, seed variables, no-default behavior, pinned component inputs and smoke execution. Write `docs/implementation/E0.md`.
- [ ] Review the final diff, check AQ inbox once for owner designation, reconcile all acceptance items and required subtasks.
- [ ] Squash implementation history before the first passing feature close, merge `origin/dev` as required, rerun targeted checks, guarded push, and close with exact evidence.

### Task 4: Contract reconciliation after merging main

Owner commit `86ca279` adds explicit requirements beyond the held task's initial
description. This section supersedes Task 1's shared-report map and corresponding
file changes: `/readyz` and `aq_release::ReadinessReport` remain unchanged for
installed updaters. `/dependency-readyz` and `aq-server dependency-readiness`
provide the bounded dependency report used by development healthchecks.

- [ ] RED: configured unavailable dependencies must leave legacy `/readyz` at 200
  with its original fields, while the new loopback endpoint returns 503.
- [ ] Restore the shared updater report and expose the separate endpoint/client;
  verify loopback fencing, pure CLI JSON and all existing endpoint tests.
- [ ] RED: real Compose must show Graph database owner `aq_graph:false`.
- [ ] Give Graph its own non-superuser database role and private password;
  refuse an existing database with a different owner rather than altering it.
- [ ] RED: Compose must use the server image for Git, with matching shared
  private paths; run the prescribed Git executable explicitly.
- [ ] Rebuild the server image, run named acceptance and focused compatibility
  tests, update evidence and record the supervisor's Git UID/image/path ruling.

Supervisor message `msg-f3bde02e8fee4a90a47aa21ecf37b623` explicitly replaces the
absent `origin/dev` merge step with `origin/main`; this branch merged it without
changing the default branch.


### Task 5: Conform to UID 10001 without a development exception

The fixed UID is already specified; satisfying it removes the pending exception
rather than waiting to waive a clear contract. Preserve the existing designated
stack during qualification; do not chown its host state or delete its volume.

- [ ] RED: named smoke requires server and Git user `10001:10001`; expect current
  host UID to fail before starting disposable containers.
- [ ] Add a bounded one-shot root initializer for new private named volumes:
  generated vault, server token copy, shared Git/source root and Unix socket.
  Runtime services stay UID 10001, read-only, cap-drop ALL. Refuse changed
  retained config/token; never recursively chown existing source data.
- [ ] Mount Git and staging under one `/var/lib/aq` volume for atomic rename;
  share `/run/aq-git`. Keep original host credentials private and unchanged.
- [ ] Run seed requests through `aq-git --socket ... --input - --json request`
  under Compose exec as UID 10001, retaining bounded/correlated response checks.
  Observe diagnostic PID through the container rather than host-owned state.
- [ ] GREEN: named smoke verifies numeric runtime users, 0700 directory owners,
  seed replay/refusals, Git degraded/restarted-ready, Graph outage and cleanup.
  Python bootstrap checks remain green. Rust code/image is unchanged.
- [ ] Qualify a fresh designated stack with preserved old stack, retain its base,
  update I0 handoff/evidence, commit and guarded publish. Resolve attempt-specific
  export boundary without creating a fake execution attempt.
