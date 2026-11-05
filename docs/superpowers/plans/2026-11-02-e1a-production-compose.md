# Production Compose Implementation Plan

> **For agentic workers:** Execute this plan natively in the assigned AQ worktree, following the task's complete-in-session instruction.

**Goal:** Run server, PostgreSQL, graph, supervised Git and cloudflared from digest references with production authentication.

**Architecture:** Promote E0 service shapes into production Compose, retain a development overlay, and provision independent non-superuser database owners. Reuse the pinned server image's A3b Git binary and E0 dependency readiness; initialize private project volumes before either consumer starts.

**Tech Stack:** Docker Compose, POSIX shell, Python scripted acceptance.

**Spec:** E1a in `2026-11-02-v2-full-application-tasks.yaml`; held task smart-falcon-16.2.

## Global Constraints

- Each repository builds alone, with no sibling dependencies or floating release inputs.
- Keep production authentication, internal database networking, no server host ports, least privilege, and rotated logs.
- Keep the updater's `server` service and format-1 image selection.
- Never log secret values; use secret files and private state.
- Evidence records exact checks, environment, commit, and limits in `docs/implementation/E1a.md`.
- Dashboard packaging, worker services, real VPS configuration, backups and updater format 2 are out of scope.

## Review Focus

- Missing graph or Git must degrade dependency readiness (smoke stops and restarts each).
- Existing database ownership must not be silently reassigned (init refuses mismatch).
- Development overlay must preserve dev auth, seed setup and UID ownership (existing dev tests and Compose config).
- Image identity must include immutable digests; Git binary must match A3b (smoke checks config and binary checksum).
- Disposable smoke must remove only its own containers and volumes, including on startup failure (finally cleanup).

### Task 1: Production assembly and scripted acceptance

**Files:** modify `compose.yaml`, `compose.dev.yaml`, `scripts/compose-smoke.py`; create `scripts/compose-prod-smoke.py`, production init scripts, `vault.production.example/config.toml`; update `docs/operations.md` production installation section.

**Interfaces:** consume `aq-server dependency-readiness --vault /vault`, `aq-git --socket /run/aq-git/central.sock --json capabilities`, E2 graph image, A3b binary. Produce `compose_prod_reaches_ready_with_graph_and_git()` through `scripts/compose-smoke.sh prod`.

- [x] Write the named production acceptance first: validate all service image digests, production Access vault, network/hardening, private shared Git state, separate DB roles, healthy dependencies, dependency outages/recovery, and cleanup.
- [x] Run `scripts/compose-smoke.sh prod`; expect missing production service/config assertions before containers start.
- [x] Promote graph and supervised Git, pin postgres/cloudflared/E2 graph, provision separate server/graph databases and private volumes. Preserve development overlay behavior. Provide an operator production vault and document secrets/digest inputs.
- [x] Run `scripts/compose-smoke.sh prod`; expect named PASS with readiness and image evidence. Run `aq test scripts/test-dev-stack.py scripts/test-compose-prod.py` for affected bootstrap/config behavior.
- [x] Run syntax/config checks; Rust source is untouched, so use focused readiness/config area validation rather than a full workspace suite per AQ prime.
- [x] Record evidence, self-review against scope, commit one feature change, merge `origin/main` as required by the project prime override, guarded push and explicit task close/drain.
