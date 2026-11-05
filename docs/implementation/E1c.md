# E1c supervisor host and cloud worker Compose evidence

Task: `smart-falcon-16.4`. Completed 2026-11-03 (America/Los_Angeles).

**Historical evidence:** the original 8 vCPU / 16 GB limits and measurements below
are superseded for deployment by [0005 §9.1](../architecture/0005-sandboxed-execution-and-deployment-topology.md#91-service-limits-and-aggregate-sandbox-budget),
which budgets the owner's 4 vCPU / 8 GB NVMe 8 VPS. The old load run does not
qualify the revised limits.

Worker-image follow-up: [E1c worker image](E1c-worker-image.md) replaces the
0.3.1 candidate and extends `prod-workers` to start the real cloud worker.
The limitations below describe this original task's evidence; the follow-up
records the additional runtime verification and the remaining boundaries.

This task makes four changes:
- Production Compose gets `aq-supervisor-host` and `aq-worker-cloud` in the overlay `compose.workers.yaml`.
- Every production service gets CPU, memory (no swap beyond memory) and PID limits.
- The sandbox daemon's aggregate cap ships as a systemd slice template: `deploy/systemd/aq-sandbox-user.slice.conf`.
- A scripted acceptance runs sandbox CPU and memory load through the rootless sandbox daemon. While it runs, the production stack's health and readiness must stay within their deadlines.

## Decision: how the sandbox daemon runs on the VPS

**Decision:** the sandbox daemon is a host rootless daemon run by a dedicated VPS user. It is not a Compose service. The daemon is rootless Podman 5 or later, the setup qualified by C2; rootless dockerd is an equivalent choice.

**Rejected: running the daemon as a Compose service.** I tested `docker:29.3.1-dind-rootless` as a service:
- It starts without `--privileged`, but only with `seccomp=unconfined`, `apparmor=unconfined`, `systempaths=unconfined`, `/dev/net/tun` and `/dev/fuse`.
- It reports `name=rootless` but `CgroupDriver none`. The nested daemon therefore has no cgroup delegation and silently discards each sandbox's `NanoCpus`, `Memory` and `PidsLimit`. That fails SBX10.
- By default it also listens on TCP 2375.

**How the host daemon is set up:**
- The dedicated user (for example `aq-sandbox`, with linger enabled) runs `podman system service` with the `systemd` cgroup manager on cgroup v2. Its socket is at `/run/user/<uid>/aq-sandbox/podman.sock`. The desktop helper `aq-sandbox-podman.sh serve` uses this same shape.
  - Verified here: Podman `5.8.8` reports `SecurityOptions` containing `name=rootless`, `CgroupDriver systemd` and `CgroupVersion 2`.
- **Aggregate cap.** Install `deploy/systemd/aq-sandbox-user.slice.conf` as `/etc/systemd/system/user-<uid>.slice.d/50-aq-sandbox.conf`.
  - Every process of that user is charged to this slice: the API service, conmon and each sandbox. So the per-sandbox C2 limits can never add up to more than the slice.
  - The slice drop-in works without any `CgroupParent` support in docker-local.
- **Worker access.** `aq-worker-cloud` bind-mounts only that socket directory at `/run/aq-sandbox` and runs as the sandbox user's UID:GID.
  - The host's main Docker socket is mounted into no service. The smoke rejects `docker.sock`, `/run/docker` and `/var/run` mounts on every service.
  - The docker-local backend independently refuses `/var/run/docker.sock` and `/run/docker.sock`, and it refuses any daemon whose info lacks `name=rootless`.

## Services

The overlay is used with the base file, e.g. `COMPOSE_FILE=compose.yaml:compose.workers.yaml`. The updater's `[compose] files` list needs the overlay added too.

**Why an overlay, not profiles.** A `${VAR:?}` image on a profiled service breaks `docker compose config` for the base stack, and the format-1 updater and E1a depend on that base. The base service set is unchanged; `test_worker_overlay_limits_and_socket_boundaries` asserts this.

### `aq-supervisor-host`

| Aspect | Setting |
|---|---|
| Image | Digest-pinned `AQ_SUPERVISOR_HOST_IMAGE` (the F1a `Dockerfile.supervisor-host`). |
| Identity | Runs as root, which F1a requires for credential separation to UID 10001. |
| Capabilities | `cap_drop: ALL`, then adds back only CHOWN, DAC_OVERRIDE, FOWNER, KILL, SETGID and SETUID. |
| Hardening | Read-only root, `no-new-privileges`, private PID namespace, `init`. |
| Shutdown | `stop_grace_period: 45s`, which is more than the 10s `wait_stopped` budget plus DB timeouts. |
| Networks | `database` plus its own `supervisor` egress network for provider access. |
| Worker role | None: no Docker socket and no worker enrollment. |
| Credentials | The root-owned config directory (`CONFIG` and `secret_file`) is mounted read-only into this service only. |
| Persistent state | `/host`, `/workspace` and `/native-homes` volumes. |

### `aq-worker-cloud`

| Aspect | Setting |
|---|---|
| Image | Digest-pinned `AQ_WORKER_IMAGE`, an aq-worker build that includes docker-local (C2/B5b). |
| Command | `run --controller-config /config/controller.json` |
| Server connection | `AQ_SERVER_URL=https://${AQ_PUBLIC_HOSTNAME}`, the public hostname behind Access. |
| Networks | Only its own `workers` egress network, never `edge` or `database`. |
| Sandbox access | `DOCKER_HOST` set to the sandbox socket. |
| Persistent state | Journal volume `/state` and credential-lease cache volume `/var/lib/aq-worker/leases`. |
| Config | Read-only `/config` bind. |
| Secrets | No Compose secrets and no supervisor mounts. |

## Limits (8 vCPU / 16 GB VPS, 0005 §9)

Each service's swap allowance equals its memory limit (`memswap_limit` = `mem_limit`), so no service can swap.

| Service | cpus | memory | pids | Reasoning |
|---|---|---|---|---|
| postgres | 1.0 | 1g | 256 | Shared by server, graph and supervisor. One full core keeps `pg_isready` and query latency independent of sandbox load. The memory covers shared_buffers and working sets for a small deployment. The PID limit covers the connection backends. |
| server | 1.0 | 1g | 512 | Serves the API and the readiness probes. Healthchecks time out at 5s, so it needs a guaranteed core. Its tokio threads and git helper processes fit within 512 PIDs. |
| aq-graph | 0.5 | 512m | 256 | Light HTTP/graph service with a small working set. |
| aq-git | 0.5 | 512m | 512 | The supervisor plus per-request `git` children; it is I/O bound. The higher PID limit is for git process fan-out. |
| cloudflared | 0.25 | 128m | 128 | Tunnel proxy. |
| graph-db-init, server-db-init, prod-state-init | 0.25 | 128m | 64 | One-shot shell, psql and chown jobs. |
| aq-supervisor-host | 0.5 | 2g | 512 | Central sessions and the guardian, tmux and native harness CLI. Mostly waiting on the provider, so it needs little CPU, but the native CLI and Node runtimes need memory. |
| aq-worker-cloud | 0.25 | 256m | 256 | The controller only. Sandbox work runs in the sandbox daemon, outside this container. |
| sandbox user slice | `CPUQuota=400%`, `CPUWeight=50` | `MemoryMax=8G`, `MemorySwapMax=0` | `TasksMax=4096` | Enough for two concurrent Rust-building sandboxes (§9). The lower weight also yields under contention. |

**Budget.** These numbers are asserted by the smoke and by pytest.
- **CPU:** the long-running services are capped at 4.0 vCPU and the sandbox slice at 4.0 vCPU, for a total of 8.0, the VPS size.
- **Memory:** the services total 5.375 GiB and the slice 8 GiB, for a total of 13.375 GiB. That leaves about 2.6 GiB for the host kernel, dockerd, Podman and page cache. The smoke requires at least 2 GiB to be left over.
- **One-shot init jobs** run before the long-running services start, so they are excluded from the CPU and memory totals.

## Environment and source

- Linux `6.18.33.2-microsoft-standard-WSL2`, cgroup v2, 24 CPUs. Docker Desktop Engine `29.3.1`, Docker Compose `v5.1.1`. Python `3.12`.
- Sandbox daemon: rootless Podman `5.8.8` at `/run/user/1000/aq-sandbox/podman.sock`. It runs from `aq-sandbox-podman.sh serve` with the systemd cgroup manager.
- systemd user manager 255. `user-1000.slice` delegates cpu, memory and pids.
- Sandbox image: `docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec`.
- Commits: see "Verified commit" below. The final publication squashes the task's own commits; its SHA is in the AQ close receipt.

## Commands and results

```sh
scripts/compose-smoke.sh prod-workers
```

Log: [E1c-checks/prod-workers-smoke.log](E1c-checks/prod-workers-smoke.log). Result: `PASS sandbox_load_does_not_starve_server_health`.

**Sequence the smoke follows:**
1. Refuse unless `AQ_SANDBOX_SOCKET` (default `$XDG_RUNTIME_DIR/aq-sandbox/podman.sock`) is not a main Docker socket and reports `name=rootless`, `CgroupDriver systemd` and cgroup v2.
2. Statically validate `compose.yaml` + `compose.workers.yaml`, using placeholder digests:
   - every service has limits, digest pins and hardening;
   - no Docker socket mounts;
   - the worker's mounts, network, URL and UID, and the supervisor's mounts, networks, capabilities and grace period;
   - no service shares supervisor sources;
   - the CPU and memory budget fits the VPS.
3. Build and pin the server image and start the E1a stack, reusing E1a's `production_stack`.
4. Check that the live `HostConfig` for postgres, server, aq-graph and aq-git carries the declared NanoCpus, Memory, MemorySwap and PidsLimit.
5. Start a transient user slice with the template's exact properties. It stands in for the `user-<uid>.slice` drop-in, because this host's sandbox user is the shared login user.
6. Launch sandboxes through the sandbox daemon's compat API with the C2 policy shape (CapDrop ALL, no-new-privileges, NanoCpus, Memory = MemorySwap, PidsLimit 64, read-only root) and `CgroupParent` set to that slice:
   - 4 sandboxes with 2 CPUs each, running 4 spinners each, for 8 CPUs of demand against a 4-CPU quota;
   - 2 memory hogs (`tail /dev/zero`, 512 MiB each).
7. Sample once a second for 30s.

**Results:**
- `dependency-readiness` exited 0 every time; the slowest of 30 samples took 0.66s against the 5s deadline.
- postgres, server, aq-graph and aq-git stayed `healthy` with `FailingStreak` 0 throughout.
- The slice used 4.0 CPUs against a 4.0 quota, which is real load held at the cap.
- Both hogs were killed (`OOMKilled` and exit 137).
- Cleanup removed the sandboxes, the slice and all project containers and volumes.

**Other checks run:**
```sh
aq test scripts/test-compose-prod.py   # 9 passed, includes the new overlay contract test
scripts/compose-smoke.sh prod          # PASS compose_prod_reaches_ready_with_graph_and_git
scripts/compose-smoke.sh dev           # PASS compose_dev_reaches_ready_with_graph_and_git
```
- `test-compose-prod.py` covers the E1a config contract plus the new overlay contract test.
- The prod smoke was re-run because its setup moved into `production_stack`.
- The dev smoke was re-run because the base limits now apply under the dev overlay too.

## Limits of this evidence

- **New services not started.** The smoke validates both new services statically but does not run them.
  - No qualified aq-worker image with docker-local exists yet: aq-worker `packaging/worker/Dockerfile` (E6a) is not merged, and the vendored 0.3.1 `Dockerfile.worker` predates C2/B5b.
  - Supervisor runtime config and session lifecycle belong to F1a/F1b.
  - Neither service's capability set is runtime-qualified here.
- **Load comes straight from the API.** Sandbox load is generated through the sandbox daemon API rather than through the worker. That is the boundary this task owns.
- **Large test host.** It has 24 CPUs and about 32 GB, so the stack would not be starved even without caps. The evidence that the caps protect the stack is indirect:
  - the slice held sandbox CPU at its quota and the hogs were OOM-killed;
  - the engine applied the stack's own caps.
  - An 8 vCPU run belongs to E5's VPS qualification.
- **Aggregate memory cap not exercised.** The 8G `MemoryMax` was not filled; memory confinement was shown only with the per-sandbox limit.
- **Slice stand-in.** The smoke used a transient slice parented under the user manager, not a system `user-<uid>.slice` drop-in, because that needs root. E5 installs the drop-in on the VPS.
- **Operator documentation** for enrollment, ceilings and installing the slice and the overlay in the updater's compose file list is E6b's.

## Follow-ups

- **Worker image.** Filed under smart-falcon-16: replace or re-pin `Dockerfile.worker` past C2/B5b, or consume aq-worker `packaging/worker/Dockerfile`. This answers the E6a comment on this task. Re-vendoring was out of scope here.
- **Stale design docs.** A docs task covers 0005 §3/§9, which still say "aq-worker-cloud (backend boat)". The proposed text: `aq-worker-cloud` uses docker-local against a dedicated host rootless Podman daemon, and the aggregate cap is a `user-<uid>.slice` drop-in.
- **Cross-repo (aq-worker).** docker-local should also refuse a daemon whose `/info` reports `CgroupDriver none` or lacks cgroup v2, because dind-rootless passes the `name=rootless` check while discarding limits.

## Verified commit

The committed smoke log was produced from clean commit `da8cd5ae` (project `aq-e1c-70cf6ded54f2`). The published commit squashes it with the plan and the evidence; its SHA is in the AQ close receipt.
