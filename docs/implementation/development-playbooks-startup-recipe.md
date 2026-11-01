# Optional inert development runtime recipe

This is a preparation recipe, not a canonical configuration change. It enables
actual optional owners with a clearly labelled dormant development definition;
it does not seed completed runs, reviews or admissions. The current preview has
no PostgreSQL socket volume (`Mounts=[]`), so its existing TCP connection cannot
enable this profile. Keep the capability unavailable until a proper trusted
socket deployment exists. Do not add a forwarding proxy, change the existing
test container or socket permissions, or weaken the transport checks.

## Required operator preflight

Provision an actual PostgreSQL Unix socket volume for the same physical cluster
and database used by the daemon. The absolute directory must not be group/world
writable and must belong to root or the configured PostgreSQL socket UID. The
entry `.s.PGSQL.<port>` must be a real socket, not a symlink, with that exact UID.
The daemon pins the directory and rechecks its inode/device before connecting.
Use the Unix listener's actual port, not the host's published TCP port. A matching
database name alone does not prove physical cluster identity; verify both
connections against the actual cluster before activation.

All DSN, key and artifact files must be absolute, owned by the daemon UID,
regular single-link files, mode 0600, outside agent-writable roots. Provision
once and retain the same namespace, generation, grant UUID, key IDs, key bytes,
binding and absolute grant deadline on restart. Do not regenerate keys or extend
the deadline as a restart step. Keep prior decryption keys when persisted
reviews/artifacts require them; a new key must not orphan retained records.

Read-only preflight on the canonical human API observed:

- Owner: `f4c33664-c524-4395-815f-5af6109a418b`.
- Project: `00000000-0000-0000-0000-000000000001`, current revision `2`.
- Workspace: `00000000-0000-0000-0000-000000000003`.
- Existing task: `563c8853-4311-54f5-bab4-505c5b18a2b2`.

Recheck those current facts at provisioning. Generate a fresh namespace UUID
(generation 1) and grant UUID. The finite grant schema requires a recipient and
at least one capability, so bind that existing task as an **unused** recipient
and grant only `operation.get`. Do not grant `message.send` or `decision.request`.
The inert artifact requires no capabilities and invokes no command.

## Definition and private files

`cargo run -p aq-server --example development_inert_playbook --locked --offline`
prints a deterministic artifact produced by the same helper used by the
no-admission fixture. Redirect it into a newly provisioned mode-0600 private
artifact file. It contains one explicit `Return(ok, null)` entry, no registry
commands/events, no triggers, dependencies or captured values. Its compiler and
purpose explicitly identify it as development inspection data. The example
prints only artifact JSON; it does not connect to a service or create a run.

Generate two independent 32-byte random secrets locally: a raw bridge-protection
key file and a separate engine-protection key encoded as lowercase hex in the
private JSON keyring. Never print key bytes or place them in this repository.
Engine keyring shape:

```json
{"active_key_id":"development-engine-1","keys":{"development-engine-1":"<64 lowercase hex characters from a fresh 32-byte secret>"}}
```

The private engine DSN must explicitly use the real socket directory, actual
Unix port, current database/user and protected credentials, with `sslmode=disable`.
This is local Unix IPC; it is not an authorized plaintext TCP fallback. No DSN
or key is passed to a worker or browser.

## Optional section template

Replace every marked value with its actual provisioned value before parsing.
This template must not be appended to the current canonical config while the
required real socket is absent. Set an absolute grant deadline between now and
24 hours from now at first provisioning, for example now plus one hour.

```toml
[playbooks]
enabled = true
profile = "human_review_v2"
engine_connection_file = "<absolute private Unix DSN file>"
socket_owner_uid = 999 # Replace with the actual socket UID; do not assume 999.
engine_keyring_file = "<absolute private engine JSON keyring>"
drive_interval_ms = 60000

[playbooks.namespace]
namespace_id = "<fresh namespace UUID, retained on restart>"
generation = 1
name = "Development inspection only"
workspace_id = "00000000-0000-0000-0000-000000000003"
project_id = "00000000-0000-0000-0000-000000000001"
owner_user_id = "f4c33664-c524-4395-815f-5af6109a418b"

[playbooks.protection]
active_key_id = "development-bridge-1"
[[playbooks.protection.keys]]
id = "development-bridge-1"
file = "<absolute private raw 32-byte bridge key>"

[[playbooks.deployments]]
artifact_file = "<absolute private inert artifact JSON>"
binding = "development-inspection-only"
grant_id = "<fresh grant UUID, retained on restart>"
expires_at_ms = 0 # Replace with the fixed valid first-provisioning deadline.
[playbooks.deployments.scope]
workspace_id = "00000000-0000-0000-0000-000000000003"
project_id = "00000000-0000-0000-0000-000000000001"
capabilities = ["operation.get"]
[[playbooks.deployments.scope.recipients]]
kind = "task"
workspace_id = "00000000-0000-0000-0000-000000000003"
project_id = "00000000-0000-0000-0000-000000000001"
task_id = "563c8853-4311-54f5-bab4-505c5b18a2b2"
```

## Actual startup effects and acceptance

Startup checks the ordered AQ schema/project/Owner, initializes its AQ namespace,
acquires the protected component namespace/lease, records runtime incarnation,
creates the bounded bootstrap grant and installs the artifact. It creates or
verifies the activation binding and retained catalog history, then records loaded
metadata, runs recovery and starts bounded periodic quanta. Component setup may
initialize its own schema/projections. These are real metadata/ownership writes;
the recipe does not claim zero database writes.

Startup does **not** itself admit a workflow. A fresh never-used namespace is
required because recovery/quanta can process previously retained admissions,
intents, decision completions or outbox events. The supplied definition has no
trigger or command and the new namespace has no such retained work. Periodic
quanta still read/lock owner state and may advance storage revisions, but must
not create runs, decisions, Graph mutations, execution attempts, mail or external
deliveries. No admission, drive or recovery POST is part of this recipe.

After staging with an actual socket and exact assets/binary, inspect real
Definitions, Runs and Decisions through their mounted human APIs. Expect the
labelled startup definition and an empty fresh-namespace run list. Decisions
shows the actual current Owner inbox; existing reviews must remain visible rather
than being deleted or fabricated to force an empty result. Verify no new
admissions/effects/outbox, attempts, Graph mutations, mailbox messages or
notification deliveries against pre-start counts, then stop/restart to check
stable pins and ownership release. Canonical promotion belongs to root.

The separate fixture
`definitions::inert_development_startup_and_quanta_create_no_work_or_delivery`
passed on combined schema35 source `8b42b08`, together with both loaded-definition
cases: three tests, zero failures, 3.30s execution (41.82s compile). The command
was `cargo test -p aq-server --test playbook_activation definitions:: -- --nocapture --test-threads=1`,
using the existing two-job/debug0 cache and disposable PostgreSQL databases.
The actual startup and two quanta admitted no work, returned empty real
Runs/Decisions, and preserved the counted execution, Graph-operation, mailbox,
notification and review records. The existing test-only Unix forwarding peer
was reused; that peer is not a deployment option or authorization to proxy the
canonical database. Log: `/tmp/aq-loaded-definitions-state35-inert.log`.

This qualifies the inert configuration's component behavior, not the missing
canonical socket deployment. No canonical database, daemon configuration,
container, socket permissions or private key files were changed by this work.
