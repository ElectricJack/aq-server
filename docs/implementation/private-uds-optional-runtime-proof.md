# Private PostgreSQL UDS optional-runtime proof

Status: source 371c7b0 reviewed; actual isolated transport and the separately
reviewed inert owner startup/read/restart now passed. Canonical optional runtime
remains disabled; the isolated AQ process is gracefully stopped.
This prepares an isolated proof only. Existing canonical AQ/Graph data, binary,
vault, container on 55432, socket permissions, proxy settings and security policy
are outside this helper's inputs and remain untouched. No Rust build is needed.

## Grounding and actual trust checks

Read with the [reviewed inert recipe](development-playbooks-startup-recipe.md),
`playbooks/config.rs`, `playbooks/runtime.rs`, and `playbooks/service.rs`.

The local cached 17-bookworm image is PostgreSQL 17.11, image ID
`sha256:99445113a4a19e82dd3713c65bd9f6fb5bf972065ab043df8d1e1c93e957da9a`,
repository digest `postgres@sha256:839fbb5a4942a7758dd93e18b42ea5649807491a0545b5eeeb8f0b8bb11857a5`.
Docker reports default seccomp/cgroup namespace policy, not rootless/userns
remapping. The host daemon user is UID/GID 1000. The existing test container's
mount list is empty, so its socket is not a host UDS deployment. Host `psql` and
`pg_restore` are absent; use the pinned image's own clients, without installing
another dependency or pulling an image.

AQ's actual loader requires one absolute Unix endpoint, explicit nonzero Unix
port, `sslmode=disable`, and no hostaddr/options. The directory must be owned by
root or the configured socket UID and not group/world writable. The endpoint must
be a real socket with that exact UID. The held directory descriptor and live
path device/inode are rechecked. Private DSN/artifact/key files must be absolute,
daemon-owned, nonempty single-link regular files, 0600, without a final symlink.

The source does not currently call `SO_PEERCRED`, and startup compares database
names rather than PostgreSQL system identifiers. These are NOT claimed as server
checks. This proof adds independent operator observations of the actual peer UID
and physical cluster ID. Both AQ's SQLx pool and the engine must use the SAME
new cluster/database; attaching a second database with the same name to canonical
AQ's old TCP pool would violate the deployment recipe even if the name check
passed. Pure transport proof does not enable an optional owner in canonical.

## Provisioning helper and bounded cleanup

[`scripts/private-uds-postgres-proof.py`](../../scripts/private-uds-postgres-proof.py)
has create/inspect/stop operations. Create always makes a new private host root;
it accepts no existing root, external DSN, canonical config or port mapping.

```sh
python3 scripts/private-uds-postgres-proof.py create
python3 scripts/private-uds-postgres-proof.py inspect --root <printed-private-root>
python3 scripts/private-uds-postgres-proof.py stop --root <printed-private-root>
```

The generated root and data/socket directories are 0700. Minimal passwd/group
files are 0600, mounted read-only only into the NEW container so numeric UID 1000
resolves as postgres. The cached official entrypoint is deliberately not invoked:
it chmods `/var/run/postgresql` to 3775 and clears PGHOST while initializing SQL.
A fixed noninteractive shell runs explicit initdb then execs the real postgres
process in the same uniquely labelled container lifecycle. It does not forward
traffic. Rootfs is read-only; caps are dropped, no-new-privileges retained, network
is none, and no host ports are published. Memory is 512MiB, CPU 1, pids 128. The only
writable binds are its new data and UDS directories; `/tmp` is a private noexec
scratch tmpfs. No Docker socket, AQ credentials or canonical directory is mounted.

The database is a fresh generated name. Local authentication is peer, host auth
rejects, `listen_addresses=''`, and socket mode 0600. The helper pins image,
container ID/label, numeric user, root inode/device and exact mount/network policy;
stop refuses a foreign resource. It stops/removes only that proof container and
retains private files/data for review. It never prunes containers/volumes or
recursively deletes an operator path. Stop the isolated AQ daemon before stopping
its database after the later runtime phase. No database restore or canonical
rollback is involved.

The helper writes separate 0600 AQ URL and engine keyword-DSN files without
printing their contents. SQLx 0.8.6 accepts an absolute `host=` URL query as its
Unix socket path; the engine loader converts the matching directory to its held
`/proc/self/fd/<fd>` path. Actual peer UID/GID 1000, 0600 socket, 0700 directory,
peer auth, no TCP listener and PostgreSQL system identifier must pass before
reporting transport success. The same cluster identifier is retained across
subsequent inspection; a changed cluster refuses.

## Pinned 35 AQ and inert-owner phase, after transport review

Use the retained state35/events binary (`8ad0aad8…1153ad`) and current qualified
0.1.2 owner, not a freshly built source 36 binary or unreleased 0.1.3 catalog code.
Select new loopback listener ports and a new private vault. Both its DATABASE_URL
and playbook engine file must point at this new UDS/database; neither references
55432 or the canonical database. No canonical backup/user data is restored.

Start the fresh AQ database once without Playbooks, bootstrap its first actual
Owner through existing development auth, record `/api/me`, then stop it. Fresh
namespace/grant UUIDs, two distinct protection keys and a fixed first-provisioning
grant deadline are generated once and retained on restart. Keep keys/DSNs out of
stdout, logs and git. Preserve the same private directory/pins on restart rather
than regenerating keys, replacing the namespace or extending the deadline.

An enabled saved project→workspace record is a structural startup requirement.
Public project PUT also needs actual catalog/node/Git/Graph verification. The
isolated transport/startup proof may use explicitly labelled structural fixture
context from the existing reviewed inert recipe, with external statuses
Unavailable and no Verified observation, source eligibility, readiness, launch or
Graph/task existence claim. This is NOT a production project installation and
cannot qualify that owner. Any minimal SQL fixture seeding is recorded separately
from runtime effects. It must create no agent attempts, Graph/task mutations,
reviews, workflow admissions or messages. The grant's unused task reference and
project/workspace association must be explicitly recorded as fixture context.
No message.send or decision.request capability is granted.

The compiled artifact must be the same reviewed development-only definition:
one `Return(ok,null)` entry, no commands/events/triggers/dependencies/captured
values, and no required capabilities. Bootstrap grant has operation.get only and
is never invoked. An actual fresh namespace must be absent before first startup;
reuse is allowed only for the exact same retained proof pins/restart. Both engine
and AQ application names must appear in this one cluster's `pg_stat_activity`,
with Unix client addresses and the same database/system identifier. This verifies
both actual clients, beyond simply storing matching DSN text.

Capture pre/post counts after fixture setup for AQ admissions/effects/outbox,
execution attempts, Graph operations, mailbox/notification delivery, reviews and
human decisions. Startup may install real namespace/grant/artifact/binding,
owner incarnation/lease and protected engine metadata; it is not zero DB writes.
Two idle quanta plus stop/restart must leave all work/effect counts unchanged.
Inspect actual loaded Definitions, empty Runs/admissions and actual Owner Decisions
through the mounted human APIs. No admission/drive/recovery POST or workflow
execution is part of the proof. Existing baseline reviews must not be removed to
force empty state. Real installed catalog/activation discovery stays its own
component-version gate.

Only isolated evidence is authorized here. Any later canonical cutover must
separately review database identity/state, complete current data preservation,
private keys, optional owner effects, served assets/binary and rollback. A new
private PG proof cannot silently change canonical's old cluster/pool pointer.

## Actual transport checkpoint

The [retained transport checkpoint](evidence/private-uds-transport/README.md)
verified one cached-image instance with unchanged reviewed policy. Real peer
UID/GID1000,0700 directory/0600 socket, peer auth and cluster identity passed.
At that checkpoint, separate private Core/engine DSN metadata agreed, and actual
AQ/engine clients had not started. Existing55432/canonical4188 configuration and
optional capability state remain unchanged through the later isolated proof.

The [unchanged typed inert artifact export and proposed fresh context](evidence/private-uds-inert-artifact/README.md)
are now retained separately. Export passed offline without building the server;
the reviewed fixture was then applied only in the fresh private database. The
[separate actual owner/read/restart evidence](evidence/private-uds-inert-runtime/README.md)
records real Unix clients, retained pins, zero work counts, actual desktop/phone
reads and final graceful ownership release. The earlier transport/artifact
checkpoints remain historical evidence of their narrower stages.
