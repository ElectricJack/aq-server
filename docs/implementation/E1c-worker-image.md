# Cloud worker image and live Compose smoke

Task: `smart-falcon-16.10`, follow-up to [E1c](E1c.md).

## Published image

```text
ghcr.io/electricjack/agent-q-worker@sha256:e20122bab7789d0ea6d267e2de4aa5032fe4eb08df2231c9a341e0d62fdfdbbb
```

The pin is also available in [`deploy/worker-image.env`](../../deploy/worker-image.env).
Load it with `--env-file deploy/worker-image.env` when using
`compose.yaml` and `compose.workers.yaml`, along with the operator's configuration.
`prod-workers` reads this file by default; `AQ_WORKER_IMAGE` can select another
digest, which must contain the identical worker candidate manifest.

| Input | Identity |
|---|---|
| aq-worker | 0.5.1 |
| Source commit | `44fe1a5e2edc10e8bfa7e6819c94c12dc81ecd12` |
| Source archive SHA-256 | `ceb76d7184091349ffac211f67599a4f2885158f13e5a968d0f312c02aa8cc52` |
| Cargo.lock SHA-256 | `8025b8ed630cd67253382b889d061fab2dfb9771a5de78abf1d44d7e6d23cbd9` |
| Foundation / execution protocol | 1 / 2, unchanged public contract bytes |
| Legacy worker / node controller journal | 4 / 1 |

This uses the task's re-pin option. The selected aq-worker main commit includes
C2's docker-local backend and B5b's controller. At implementation time E6a's
`packaging/worker/Dockerfile` was still on an unmerged worker task branch.
`Dockerfile.worker` therefore retains this repository's build and aq-git runtime
assembly, using the newer immutable source archive instead of the old 0.3.1
candidate. No sibling checkout is required to build the image. The build selects
only the production worker and existing fixture-agent binaries, without test or
synthetic runtime features. Builder and runtime base images are digest-pinned.

## Build and publication

```sh
python3 scripts/check-worker-candidate.py --official
worker_revision=$(python3 scripts/check-worker-candidate.py --official --revision)
worker_archive_sha256=$(python3 -c 'import json; print(json.load(open("vendor/worker-candidate.json"))["archive_sha256"])')
worker_version=$(python3 -c 'import json; print(json.load(open("vendor/worker-candidate.json"))["version"])')
docker build --progress=plain -f Dockerfile.worker --target worker \
  --build-arg AQ_SOURCE_REVISION="$worker_revision" \
  --build-arg AQ_WORKER_ARCHIVE_SHA256="$worker_archive_sha256" \
  --build-arg AQ_VERSION="$worker_version" \
  -t ghcr.io/electricjack/agent-q-worker:smart-falcon-16.10 .
docker push ghcr.io/electricjack/agent-q-worker:smart-falcon-16.10
```

The registry push returned the manifest digest above. Build and publication logs:
[worker-build.log](E1c-worker-checks/worker-build.log),
[worker-push.log](E1c-worker-checks/worker-push.log).
Registry manifest: [worker-manifest.json](E1c-worker-checks/worker-manifest.json).
Image identity and labels: [E1c-worker-image.json](E1c-worker-image.json).
The existing release workflow consumes the same candidate and Dockerfile.

## Runtime changes

The image sets `HOME=/state`, prepares the sandbox socket mount point, and carries
`worker-init-volumes`. Compose runs `aq-worker-state-init` before the worker to
assign the two named volume roots (journal and credential lease cache) to the
configured sandbox UID:GID with mode 0700. This fixes the image's fixed UID 10001
ownership when the sandbox daemon has a different socket owner. The initializer
has only ownership capabilities, no network and no bind mounts. It refuses root
or malformed sandbox UID/GID settings before touching either volume.

The long-running worker retains the existing limits, read-only root, dropped
capabilities, private workers network and dedicated rootless socket bind. Its
configuration bind stays read-only. Neither worker service receives supervisor
state or credentials.

## Verification

```sh
aq test scripts/test-compose-prod.py scripts/test-git-candidate.py
python3 scripts/check-worker-candidate.py --official
python3 scripts/check-git-candidate.py --official
scripts/compose-smoke.sh prod-workers
```

Focused tests: **26 passed**. Both official candidate checks passed. Results:
[focused-tests.log](E1c-worker-checks/focused-tests.log).
After fixing profiled-service teardown, the Compose area tests passed again:
**16 passed**, [compose-tests-final.log](E1c-worker-checks/compose-tests-final.log).

Live smoke: **PASS compose_worker_starts_with_docker_local** and
**PASS sandbox_load_does_not_starve_server_health**. Results are recorded in
[prod-workers-smoke.log](E1c-worker-checks/prod-workers-smoke.log).

- Worker: `aq-worker 0.5.1`, connected docker-local controller, journal persisted,
  server-recorded heartbeat, **zero restarts** during the load window.
- Journal and lease volume roots: `1000:1000:700`, matching the sandbox socket
  owner rather than the image's default UID 10001.
- Load: 30 seconds, 30 successful readiness samples; maximum latency **0.27s**
  against the 5s deadline. All four core services stayed healthy with no failing
  streak. Sandbox slice usage was **4.0 CPUs** against a 4.0 CPU quota. Both
  memory hogs exited 137 (one also reported `OOMKilled=true`).
- Cleanup verified: Compose containers/volumes, registry, temporary server image,
  rootless load sandboxes and transient slice removed. Teardown enables all
  Compose profiles because `--remove-orphans` alone leaves explicitly started
  profiled worker services behind.

The smoke runs the actual `aq-worker-cloud` service and its initializer. It
requires the pinned version and embedded candidate manifest, a connected
docker-local backend, durable controller identity/journal, named volume
ownership, an authenticated heartbeat recorded by the real server, and zero
worker restarts throughout sandbox load. Live engine limits are checked for the
worker as well as postgres, server, graph and aq-git. The existing sandbox CPU,
OOM and service readiness checks remain in place. All disposable containers,
volumes, load sandboxes and the transient slice are removed.

## Boundaries

- The smoke seeds a disposable foundation node credential directly in its own
  database, and only its server additionally joins the workers network. The saved
  worker enrollment uses `http://server:8081` for the local heartbeat. Production
  keeps the worker on its egress network and uses the public HTTPS Access URL in
  `agent-q/worker.toml`; `AQ_SERVER_URL` does not override that saved config.
  Access enrollment and the public TLS route are not qualified here.
- This worker revision deliberately uses `UnavailableProvider`: its controller
  connects to docker-local, recovers state and stays alive with foundation
  heartbeats, while B2 admission remains fenced. This task does not claim an
  end-to-end sandbox assignment or provider execution. Load still comes directly
  from the sandbox daemon API, as in E1c.
- The supervisor service remains statically validated. Its startup/session
  lifecycle belongs to F1a/F1b. E1c's large-host and transient-slice evidence
  limitations also remain.
- The image is published for Linux amd64. A production VPS rollout and release
  channel promotion are separate operations.
