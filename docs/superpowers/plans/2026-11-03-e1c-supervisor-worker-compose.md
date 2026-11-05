# E1c Supervisor Host and Cloud Worker Compose Implementation Plan

> **For agentic workers:** Execute this plan natively in the assigned AQ worktree, following the task's complete-in-session instruction.

**Goal:** Add `aq-supervisor-host` and `aq-worker-cloud` to production Compose and put CPU, memory and PID limits on every production service, so sandbox load cannot starve postgres, server, graph, aq-git or cloudflared.

**Architecture:**
- `compose.yaml` keeps the E1a stack and gains `cpus`, `mem_limit`, `memswap_limit` and `pids_limit` on every service.
- A production overlay, `compose.workers.yaml`, adds the two new services. It must be used together with `compose.yaml`, e.g. `COMPOSE_FILE=compose.yaml:compose.workers.yaml`.
  - The services go in an overlay, not behind profiles. A `${VAR:?}` image on a profiled service breaks `docker compose config` for the base stack, which the updater and E1a depend on.
- The sandbox daemon is a host rootless Podman ≥5 run by a dedicated VPS user. It is not a Compose service.
  - Its aggregate cap is a systemd drop-in on `user-<uid>.slice`, shipped as a template in `deploy/systemd/`.
  - `aq-worker-cloud` receives only that daemon's socket directory.

**Tech Stack:** Docker Compose, systemd user slices, rootless Podman compat API, Python scripted acceptance.

**Spec:** E1c in `2026-11-02-v2-full-application-tasks.yaml`; held task smart-falcon-16.4.

## Decisions

- **Sandbox daemon.** It runs on the host as a rootless daemon for a dedicated user.
  - Tried and rejected: running it as a Compose service (`docker:dind-rootless`). It starts without `--privileged` only once seccomp, apparmor and systempaths are set to unconfined, and it then reports `CgroupDriver none`. As a result it silently discards each sandbox's NanoCpus, Memory and PidsLimit, which violates SBX10.
  - Host Podman 5.8.8 reports `name=rootless`, the systemd cgroup driver and delegated cpu, memory and pids controllers.
- **Worker connection.** `aq-worker-cloud` runs as the sandbox user's uid:gid, the uid that owns the socket.
  - It sits on its own egress network, `workers`, and is never on `edge` or `database`.
  - It reaches the server at `https://${AQ_PUBLIC_HOSTNAME}` through Access.
  - The image is a digest-pinned `AQ_WORKER_IMAGE` built from aq-worker packaging. The vendored 0.3.1 `Dockerfile.worker` predates docker-local; a follow-up task covers it.
- **Supervisor host.** `aq-supervisor-host` has no Docker socket and no worker enrollment.
  - It runs on the `database` network plus its own `supervisor` egress network.
  - Its credentials are mounted only into that service.
- **Smoke scope.** The smoke does not start either new container. Their runtime config belongs to F1a and E6b, and no qualified worker image exists yet. Instead, the smoke drives load directly through the sandbox daemon, which is what the acceptance requires.

## Limits (8 vCPU / 16 GB VPS, 0005 §9)

| Service | cpus | memory (swap = memory) | pids |
|---|---|---|---|
| postgres | 1.0 | 1g | 256 |
| server | 1.0 | 1g | 512 |
| aq-graph | 0.5 | 512m | 256 |
| aq-git | 0.5 | 512m | 512 |
| cloudflared | 0.25 | 128m | 128 |
| init jobs (x3) | 0.25 | 128m | 64 |
| aq-supervisor-host | 0.5 | 2g | 512 |
| aq-worker-cloud | 0.25 | 256m | 256 |
| sandbox user slice | CPUQuota=400% | MemoryMax=8G, MemorySwapMax=0 | TasksMax=4096 |

**How the numbers add up:**
- CPU: the long-running stack is capped at 3.25 vCPU. Sandboxes, supervisor and worker get 4.75 vCPU, for a total of 8.
- Memory: the caps total about 13.4 GB, which leaves about 2.6 GB for the host and page cache.

## Task 1: Scripted acceptance first

**Files:**
- Create `scripts/compose-workers-smoke.py`.
- Add the `prod-workers` dispatch in `scripts/compose-smoke.py`.

**Steps:**
- [ ] Write `sandbox_load_does_not_starve_server_health()`.
  - Refuse to run unless the socket is a rootless sandbox socket with the systemd cgroup driver and is not the main socket.
  - Validate the merged production config statically:
    - Every service has limits.
    - No service mounts a Docker socket.
    - The worker mounts only the sandbox socket directory, uses no edge or database network, and points at the public URL.
    - The supervisor has no socket, runs in a private PID namespace, and has a grace period of more than 10s.
  - Start the E1a stack and check that the live HostConfig matches the limits.
  - Inside a transient user slice that stands in for the production slice drop-in, launch CPU spinners and memory hogs through the daemon API with C2-style per-sandbox limits.
  - For the whole load window, require `healthy` for the server, postgres, graph and aq-git, and `dependency-readiness` exit 0 within 5s.
  - Require that the load was confined: slice CPU at or below the quota, and the hog OOM-killed.
  - Clean up everything.
- [ ] Run `scripts/compose-smoke.sh prod-workers`. Expect it to fail on the missing limits or overlay before any container starts.

## Task 2: Compose and slice template

**Files:**
- Modify `compose.yaml`.
- Create `compose.workers.yaml` and `deploy/systemd/aq-sandbox-user.slice.conf`.
- Update `docs/operations.md`.

**Steps:**
- [ ] Add the limits, the overlay services and the slice template.
- [ ] Run `scripts/compose-smoke.sh prod-workers` and expect a named PASS. Run `aq test scripts/test-compose-prod.py` to confirm the E1a config contract still holds.
- [ ] Record evidence in `docs/implementation/E1c.md`: commands, environment, commit, limits with reasons, the decision, and the limits of the evidence.
- [ ] File follow-ups: a re-pin of Dockerfile.worker, a docs task for the stale 0005 "backend boat" wording, and a cross-repo note that docker-local should refuse `CgroupDriver none`.
- [ ] Squash, merge `origin/main`, push with guarded `aq git push`, then close.
