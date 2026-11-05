# E1d — independent Boat worker in production Compose

The `boat` profile in `compose.workers.yaml` adds `aq-worker-boat` and its
networkless state initializer. Use it with `compose.yaml`. The same service
definition is intended for the desktop production deployment first (E5a)
and VPS later (E5b), per the owner's 2026-11-03 re-scope.

The node has a separate enrollment/config bind, persistent controller and
Boat mapping volume, lease volume, and attempt-key secret. It has no local
sandbox socket, central database network or host port. Node enrollment uses
the machine HTTPS hostname and its own Cloudflare Access service token;
runners need their separate token. The cloud node remains docker-local.

The entrypoint defaults to `configuration-required`, with no enrollment,
backend calls or sandbox creation. Explicit enablement requires a separate
C7-capable image, controller config, saved enrollment and nonempty attempt
key. The key is copied to same-owner mode-0600 tmpfs for C7 `BoatKey`.
Controller and provider mapping paths share only this node's persistent
state volume. Examples use C7 `RunConfig`/`BoatConfig` fields, distinct
attempt/build organizations and a build-only key kept off the worker.

Limits: controller 0.125 CPU, 128 MiB memory/swap ceiling, 128 PIDs;
initializer 0.25 CPU, 128 MiB memory/swap ceiling, 64 PIDs. C7, rather than
Compose shutdown, owns hard-deadline VM TTL and controller recovery. The
example retains one-second sweep and five-second deadline margins.

The existing NVMe 8 budget remains unchanged: 4 CPUs and 6.5 GiB total
service/sandbox ceilings, plus at least 1 decimal GB host headroom. Adding
Boat requires 4.125 CPUs and 6.625 GiB, so the profile is opt-in and the
existing small-VPS budget rejects it. The desktop smoke checks the Docker
engine host's actual CPU/memory capacity. E5b must establish a fitting
aggregate budget before enabling this profile on the VPS.

## Verification

- `aq test scripts/test-compose-prod.py -q`: **19 passed**. Covers the
  existing base/dev/worker isolation, strict small-VPS budget, Boat profile
  identity/state/secret separation and advertised $18 / four-sandbox limits.
- `scripts/compose-smoke.sh prod-boat`: **PASS boat_configuration_required**.
  Ran the actual service and initializer with the qualified existing worker
  image (only the credential-free shell placeholder). Docker host: 24 CPUs,
  33,652,641,792 bytes RAM. Runtime UID 10001; state and lease directories
  10001:10001:0700; engine limits match 0.125 CPU, 134,217,728 bytes
  memory/swap and 128 PIDs. Controller/mapping fixture files survived
  restart. The key was not installed. All owned containers, volumes and
  network were removed. No Boat provider request or enrollment occurred.
- `sh -n scripts/boat-entrypoint.sh` and `git diff --check`: passed.

The first focused run correctly rejected the expanded profile against the
fully allocated small-VPS budget. The final profile preserves that rejection;
the desktop capacity check does not relax the default VPS guard.

## Deployment dependencies and supervisor dispositions

Supervisor message `msg-d9cd37763ce940d38b273942f19dda06` explicitly directs
closing this implementation without waiting for these follow-ups:

1. **Separate C7 image.** Require `AQ_BOAT_WORKER_IMAGE` as an immutable
   manifest digest built from C7 commit `c009b6d`; leave the qualified cloud
   image unchanged. The supervisor reports C7 completed on its published
   task branch. Image release publication follows the owner's C7 PR merge.
   This task has not built/published a C7 release image or qualified live
   provider operation. The existing vendored 0.5.1 worker cannot run Boat.
2. **Monthly VM accounting.** The example advertises USD 18,000,000 micros
   and max concurrency four. H2 currently prices lifetime token-call facts;
   it has no monthly Boat VM-time schema/feed. The supervisor is filing
   that as a separate follow-up. The operations guide records the owner
   rates (small $0.018/h; default $0.036/h), placement refusal reasons, and
   the deployment gate. The $20 plan, auto top-up off, extra starts off,
   member usage cap, concurrency four, and at most ten saved templates are
   operator-side prerequisites, not changes made to a live Boat account.

Access enrollment, runner/admission readiness, live Boat/template
qualification, monthly accounting and desktop/VPS provisioning are not
claimed by the placeholder smoke. Defaults leave attempt execution disabled.
