# Deploying AQ on Google Cloud: what it takes

Status: a plan, written 2026-11-17. **Nothing here has been deployed or tested on
Google Cloud.** The facts about AQ come from reading the code and the running
development stack; each section says what is established and what is assumed.
The local setup this builds on is `docs/getting-started.md` (pull request #149).

## Summary

AQ can run on Google Cloud today as **one control-plane VM plus worker VMs**,
with one piece of new code: a way for people to sign in. Everything else is
packaging and operations.

It cannot run as a set of stateless, autoscaled services (Cloud Run, or several
server replicas behind a load balancer) without design work, for three reasons
that come from how AQ is built:

1. The server is a single instance. Request limits, terminal streams and several
   caches live in the server process, and every instance would run every
   background sweep.
2. The server and `aq-git` share a Unix socket and a local volume, so they must
   sit on the same machine.
3. A worker needs a real Linux host: a systemd user session and a rootless
   Podman daemon with delegated cgroups. It does not run in an ordinary
   Kubernetes pod.

The recommended first target is therefore the same topology as a developer
machine, on Compute Engine, with Google's managed pieces around it.

## What has to exist first

These are prerequisites whether or not the target is Google Cloud.

| Need | State on 2026-11-17 |
| --- | --- |
| A release: tagged, published images for the server, worker pieces, `aq-graph` and the supervisor host | No version tag has been cut. `release.yml` publishes server and worker images to GHCR on a tag; it does not build the supervisor-host image. Every image in use so far was built locally. |
| A reproducible worker: template image, driver pins, `native.json` | Built by one tool for a local stack (`scripts/local-stack`); the same steps must run in a release pipeline. |
| A sign-in method that works on Google Cloud | Missing. See [Sign-in](#sign-in-the-one-piece-of-new-code). |
| A production configuration | `vault.production.example/` exists. Production mode refuses dev sign-in and requires `server.public_origin` to be `https://`. It has never been run. |

## Target architecture

```
people ── HTTPS ──> external Application Load Balancer + Identity-Aware Proxy
                         │  (one hostname, e.g. aq.example.com)
                         ▼
        ┌──────────── control-plane VM (Compute Engine) ────────────┐
        │ docker compose:                                            │
        │   server :8080 (people)  :8081 (workers)                   │
        │   aq-git   ← Unix socket + shared volume → server          │
        │   aq-graph                                                 │
        │   internal-work host, supervisors (started by a manager    │
        │   with Docker access)                                      │
        │   postgres  (phase 1)  → Cloud SQL (phase 2)               │
        │ aq-updater (host service)                                  │
        │ persistent disk: /var/lib/aq  (git repositories, artifacts)│
        └────────────────────────────▲───────────────────────────────┘
                                     │ HTTPS, outbound from workers only
        worker VMs (Compute Engine, private IPs, Cloud NAT for egress)
          systemd user service aq-node@…  +  rootless Podman daemon
          one sandbox container per agent slot ── HTTPS ──> model providers
```

### Control plane

One VM running the existing Compose files, as the production design already
assumes (`docs/operations.md`, `compose.yaml`).

- **Why one VM:** the server opens `aq-git` through `/run/aq-git/central.sock`
  and both mount `/var/lib/aq` read-write; `source_retention.staging_root` must
  be on the same filesystem as `git_root` because publication is an atomic
  rename. The internal-work host and the supervisors are started by a manager
  that needs the Docker socket and direct database access.
- **Size:** the development stack was comfortable with about 3 vCPU / 1.5 GiB
  for the server, 4 vCPU / 4 GiB for `aq-git`, 4 vCPU / 4 GiB for PostgreSQL.
  Start with 8 vCPU / 32 GiB and measure. These are single observations, not a
  load test.
- **Disk:** a persistent SSD disk for `/var/lib/aq`. The development stack holds
  18 GB there (11 GB artifacts, 6.4 GB repositories) after a few weeks.
- **Availability:** a server restart is an outage of about a minute; workers
  retry. There is no standby. Plan upgrades accordingly.

### Database

- **Phase 1:** PostgreSQL 17 in Compose on the VM's persistent disk, exactly as
  tested. `aq-updater` takes a `pg_dump` before each update and restores it if
  the new server is not ready within 120 s.
- **Phase 2:** Cloud SQL for PostgreSQL 17 over a private IP. To check before
  moving: the three databases and roles (`aq_server`, `aq_graph`, the playbook
  role), that the supervisors and the internal-work host (which connect to
  PostgreSQL directly) can reach it, the updater's dump-and-restore step
  against a remote database, and connection count (the development stack used
  about 50 of 100 with three worker nodes; pool sizes are `[workers.limits]`).
- Migrations run at server start and are one-way: rolling back across a
  migration means restoring the backup.

### Workers

A worker is a VM, not a container.

- **Image:** Ubuntu 24.04 x86-64 with `uidmap`, `iptables`, `tmux`, user
  lingering, cgroup v2 with `cpu`, `memory` and `pids` delegated. The installer
  (`aq-worker` `packaging/service/aq-install.sh`) sets up the rest, including
  its own pinned rootless Podman. No nested virtualisation is needed.
- **Size per agent slot:** 2 vCPU, 4 GiB RAM, 24 GiB disk is what a sandbox is
  given today. A node with 8 slots needs about 16 vCPU, 32 GiB plus headroom,
  and 250 GB of disk (state, the image store, and build output of the projects
  it works on). The slot count is a server Setting per node.
- **Network:** workers only dial out: to the server's worker listener, to the
  install origin, and to the model providers. Give them private IPs and Cloud
  NAT. Workers in the same VPC can reach the control plane on its internal
  address; that still needs TLS in front of port 8081 (the server speaks plain
  HTTP), for example an internal Application Load Balancer.
- **Workers outside Google Cloud** (a laptop, an office machine) need a public
  HTTPS endpoint for the worker listener. Do not put Identity-Aware Proxy in
  front of it: workers authenticate with their node credential, not as people.
- **Enrollment:** a node is enrolled with a single-use code that lasts ten
  minutes, created by an owner. That suits a handful of long-lived worker VMs.
  An autoscaled group needs enrollment that a new VM can do unattended, which
  does not exist yet.
- **Provider logins:** agents run on subscription logins (a Claude token, a
  Codex login) that an owner pushes to a node from the dashboard. Decide before
  building a fleet whether a shared subscription login on many cloud machines
  is acceptable to the provider and to you; rate limits with many agents are
  unmeasured.
- **Isolation:** sandboxes are containers sharing the VM's kernel. The design
  documents say this is not isolation against hostile code. Do not co-locate
  workers with anything sensitive, and give worker VMs a service account with
  no permissions.

### Ingress

- **People:** an external Application Load Balancer with a managed certificate,
  forwarding to port 8080. The server checks that every changing request's
  `Origin` equals `server.public_origin` exactly, so there is one hostname.
  Set the backend timeout to 300 s: terminal polls are held for up to 20 s and
  the dashboard polls rather than using WebSockets.
- **Health check:** `/healthz` only. `/readyz` answers loopback callers.
- **Install origin:** workers fetch their install bundle and the sandbox
  template image by digest from an HTTPS origin (`[workers.install]`). A
  Cloud Storage bucket behind the load balancer, or the control-plane VM, can
  serve it. The dashboard build has this URL and its checksum baked in
  (`VITE_AQ_WORKER_INSTALL_URL`, `VITE_AQ_WORKER_INSTALL_SHA256`), so the
  dashboard bundle is specific to an environment.

## Sign-in: the one piece of new code

The server has two sign-in modes: `dev` (no login; refused in production) and
`cloudflare_access` (verifies the signed header Cloudflare Access adds). There
is no login page, no OIDC and no sessions.

Two ways forward:

1. **Keep Cloudflare Access in front of Google Cloud.** No new code. A
   Cloudflare Tunnel from the control-plane VM, as the original design
   describes. This mode has not been run against a real Access application
   either.
2. **Add an Identity-Aware Proxy mode** (recommended for a Google Cloud
   deployment). IAP adds a signed `x-goog-iap-jwt-assertion` header; the server
   would verify it against Google's published keys and the backend service's
   audience, the same shape as the existing Cloudflare verifier
   (`crates/aq-server/src/identity/cloudflare.rs`). The firewall must then
   admit only the load balancer to port 8080.

Roles are Owner and Viewer; the first owner comes from `[bootstrap] owner_email`.

## Secrets and configuration

- AQ reads secrets from files (database passwords, the graph token, playbook
  keys, the terminal protection key) and its configuration from a read-only
  "vault" directory (`config.toml`, `playbooks.toml`). Keep them in Secret
  Manager and write them to those paths at boot with the VM's service account.
  No secret manager is integrated in the code.
- Rotate every credential that has existed on a development machine before it
  is used in a hosted deployment, and run the server as its own database role
  (`aq_server`), not the bootstrap role.
- Values that differ per environment: `public_origin`, the sign-in settings,
  the owner email, the graph URL, `result_media.storage_id`, the node
  allow-lists and artifact digests under `[intelligence]` and
  `[profiled_execution]`.

## Images and updates

- Publish images on a version tag and pull them by digest. GHCR works from
  Compute Engine; mirroring to Artifact Registry removes the outside
  dependency.
- `aq-updater` on the control-plane VM performs updates: pull, verify, mark
  maintenance, back up the database, replace, probe, restore on failure. It is
  the only component that should hold Docker access for updates.
- Workers are updated separately. A worker's programs and the server's
  definitions name each other's digests; update them as a pair.

## Backups

| What | How | State |
| --- | --- | --- |
| PostgreSQL | `pg_dump` by the updater before each update (keeps five); add a scheduled dump to Cloud Storage, or Cloud SQL backups in phase 2 | Off-host backup is specified, not built |
| `/var/lib/aq` (git repositories, artifacts) | Scheduled persistent-disk snapshots | No procedure exists; the database and this volume must be restored to a consistent pair |
| Vault and secrets | Secret Manager versions | — |

A restore has never been rehearsed end to end. Rehearse one before relying on it.

## Observability

- Logs are plain text on stderr; the Ops Agent or Docker's `gcplogs` driver
  sends them to Cloud Logging. Worker logs are in journald.
- There is no metrics endpoint. Until one exists, alert on `/healthz`, on 503
  responses in the load balancer's logs, and on PostgreSQL CPU and connections.
- Known load behaviour: capacity reads slow down while finished attempts wait
  to settle, and large source transfers are refused when their limit is
  reached. Both show up as 503s that clients retry. Limits and pool sizes are
  configuration under `[workers.limits]`.

## Phases

| Phase | Outcome | Main work |
| --- | --- | --- |
| 0. Release | Tagged images, reproducible worker build, production vault | Cut a tag; add the supervisor-host image to the release workflow; run production mode once locally |
| 1. One VM, one worker | AQ reachable over HTTPS with real sign-in, running tasks on one worker VM | Sign-in mode; load balancer; control-plane VM with Compose and the updater; one worker VM enrolled by hand |
| 2. Hardening | Safe to depend on | Secret Manager; backups and a rehearsed restore; Cloud SQL; logging and alerts; worker listener over TLS |
| 3. Worker fleet | Add and remove workers without hand work | Worker VM image; unattended enrollment; login delivery at scale; per-node slot Settings |
| Later | More than one server instance; workers on GKE or another sandbox backend | Design work: the process-local limits, terminal streams and sweeps listed in the summary |

## Open questions for the owner

1. Identity-Aware Proxy, or Cloudflare Access in front of Google Cloud?
2. Will workers outside Google Cloud (laptops) connect to the hosted server? It
   decides whether the worker listener is public.
3. Subscription logins on cloud workers, or provider API keys? The system is
   built around subscription logins today.
4. One shared deployment for the team, or one per person? AQ is designed as a
   single-owner deployment; there is no multi-tenant separation.
5. Where images are published, and the first version number.
