# Authenticated source retention

AQ-09 exposes bounded source transfer on the worker listener. Node credentials
are resolved through the ordinary live central verifier. The attempt owner,
repository UUID, logical identity, binding generation, base OID, session,
generation and resolved spec digest come from the server's immutable offer.
An agent graph credential cannot authenticate these routes. The adapter does
not expose Git commands, paths, URLs or publication.

Checkpoint upload uses `POST /v1/execution/sources/begin` with
`{identity,operation_id,manifest:TransferManifest}`. The manifest's role must be
`checkpoint`. `PUT /v1/execution/sources/{transfer_id}/{manifest|bundle}` accepts
raw bytes with `operation_id`, `offset` and `digest` query parameters. Each
chunk is at most 256 KiB; request bodies have a separate 1 MiB cap. Exact
operation replay returns its durable response; changed payloads or incorrect
offsets fail. Files are fsynced before SQL offsets advance. Unacknowledged
trailing bytes are truncated on retry. Reservations are durable before files
are created. The max bundle is configurable up to aq-git's 1 GiB bound.

`POST /v1/execution/sources/status` and `/finalize` accept
`TransferStatusRequest`. Status uses the reserved control database pool;
bulk requests use a separate bounded semaphore. Node and attempt locks are
released before whole-file hashing or socket I/O. Validation protects a completed upload from concurrent expiry cleanup.
Definitely invalid pre-dispatch bytes are rejected and reclaimed; a durable
dispatch-intent flag prevents reclaiming any potentially imported bytes.
The import operation UUID is allocated once in SQL and preserved across lost or incompatible responses.
Publication into the private aq-git incoming area uses an atomic directory
rename. Put `staging_root` on the same filesystem as `git_root`. Unknown imports
remain pending, including after worker disappearance; no timeout invents a
receipt or renews assignment authority.

The only source import mapping is `artifact.import` with trusted repository,
generation, durable import UUID and a UUID transport artifact handle. The
adapter validates the actual aq-git manifest and every central receipt binding.
The transport manifest digest hashes exact uploaded bytes; aq-git's retention
receipt digest hashes its typed canonical manifest serialization. These are
separate values. Upload completion never means central retention. A retained
receipt does not accept an AQ attempt or authorize candidate publication. Late
attempts can retain their own source without restoring authority.

After the complete central receipt is committed, incoming transport files are
removed and their staging reservation released. Crash recovery retries this
cleanup. Expired uploads that have never entered import may be deleted;
pending imports are protected. Staging byte and transfer count reservations
bound uploads. Operator-provisioned base exports share the byte budget;
unknown exports retain their maximum reservation. The central aq-git retention
store independently owns retained objects and their release lifecycle.

An owner provisions a frozen base through
`POST /api/source-bases/provision` with `{attempt_id,operation_id,retention_handle}`.
The only mapping is `artifact.export` of the server's frozen base using that
trusted retained handle. Keep the same operation UUID after uncertainty.
The worker gets its typed descriptor at
`GET /v1/execution/sources/base/{attempt_id}`, then downloads chunks at
`GET /v1/execution/sources/base/{attempt_id}/{manifest|bundle}?offset=N&limit=N`.
The maximum download chunk is 256 KiB. Ownership and frozen binding checks
apply to every descriptor and chunk. AQ-08 scheduling/provisioning remains
outside this operator endpoint.

# Operator startup

The server image includes the executables but does not start or supervise an
aq-git daemon. Until REL-01 provides process ownership, an operator must run
and supervise the central service separately. Run it as the same numeric UID
as aq-server (the packaged server uses UID 10001). Keep its state, incoming
artifacts, staging directory and socket private to that identity; aq-git creates
its Unix socket with mode 0600. In separate containers, mount the same private
state and socket directories at these same paths with matching UID ownership.

As that service identity, with the parent directories already provisioned:

```sh
umask 077
install -d -m 0700 /var/lib/aq/git /var/lib/aq/source-staging /run/aq-git
aq-git --state-dir /var/lib/aq/git \
  --git-executable /opt/aq-git-runtime/bin/git \
  --listen /run/aq-git/central.sock serve
```

Keep that foreground process under the operator's supervisor. From another
session with the same identity, verify its public socket before starting AQ:

```sh
aq-git --socket /run/aq-git/central.sock --json capabilities
```

Add the matching section to the vault's `config.toml`:

```toml
[source_retention]
staging_root = "/var/lib/aq/source-staging"
git_root = "/var/lib/aq/git"
git_socket = "/run/aq-git/central.sock"
max_staging_bytes = 4294967296
max_bundle_bytes = 1073741824
bulk_concurrency = 2
incomplete_ttl_secs = 86400
keep_per_task = 2
max_age = 2592000 # seconds (30 days)
```

Put both state and staging on the same filesystem for atomic publication.
Provision the central repository binding and retained base handle through the
public aq-git administrator CLI, then use the owner base-provision endpoint
above. Repository UUID, identity, generation and base must match the frozen
assignment. The server does not invent bindings, start the daemon, change
socket ownership or infer source durability when that service is unavailable.

# Reproducible aq-git package

`python3 scripts/check-git-candidate.py --official` verifies the retained
v0.2.0 release archive, executable, build record, Git bundle and complete source
inventory without reading sibling checkouts. The release tag is `v0.2.0`, at
commit `e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`. The checker compares every
archived source file with both the build record and the bundle's committed
blobs, and checks the archive's embedded commit and locked package version.

The [A3a release](https://github.com/ElectricJack/aq-git/releases/tag/v0.2.0)
executable has SHA-256
`3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2`.
`docker build -f scripts/Dockerfile.aq-git .` verifies it with `sha256sum -c`
and copies those exact bytes. The server and worker embed the same canonical
stages. Copying the released executable makes the consumer pin match the
release digest without relying on a reproducible rebuild. A3a built it with
locked dependencies in the digest-pinned Rust 1.96.1 bookworm builder retained
by these recipes; it requires at most GLIBC2.34 and executes in bookworm.
The retained executable targets Linux x86_64. The retained build record's
pending-publication fields describe its preparation time; the published tag
and fresh consumer qualification are recorded in `docs/implementation/A3b.md`.

Bookworm's distribution Git2.39 fails aq-git's Git2.43 minimum. The recipe also
builds retained upstream Git2.56.0, the latest source release listed by the
[official Git installation page](https://git-scm.com/install/) when checked on
2026-10-30. The [kernel.org archive checksum list](https://www.kernel.org/pub/software/scm/git/sha256sums.asc)
is retained alongside the exact archive. Its SHA256 is pinned in the candidate
manifest and recipe. HTTPS helpers and curl/SSL/expat/zlib runtime libraries are
included. Git helpers use a relocatable runtime prefix. Server/worker images must copy both `/usr/local/bin/aq-git` and
`/opt/aq-git-runtime`, put its `bin` first on PATH, and install the recipe's
runtime libraries. The recipes also link `/usr/bin/git` to the pinned runtime,
so the component and worker defaults select Git 2.56.0. The whole application
base image need not change.

Current release-pin evidence is in `docs/implementation/A3b.md`; earlier
qualification evidence is in `docs/evidence/m4-git-*.{json,log}`. The process/socket scenario
imports source centrally, stops and removes the complete worker volume, then
constructs a candidate from central retention. These are local qualification
results, with no hosted-production, performance or power-loss claim.

# Settled task cleanup (SBX24)

When source retention is configured, the server sweeps settled tasks on startup
and every minute, eight tasks per tick. Confirmed human or automatic task
completion, or server-owned `published` delivery evidence, makes a task eligible.
An execution result acknowledgement alone is not task acceptance. A task with
any held attempt is protected, including preserved attempts that still own
source or effects. Unknown execution authority and uncertain stops are protected
as well. Native attempts use current hot authority, not historical projections.

The newest `keep_per_task` checkpoint handles across a task's attempts remain
(default 2, ordered by transfer creation time with transfer UUID as the tie
breaker). After `max_age` seconds from the latest settlement fact (default
2592000, 30 days), every eligible checkpoint handle may be released. Zero
`keep_per_task` releases all eligible handles immediately; `max_age` must be
positive. Open tasks never expire through this policy.

Each release first inspects the handle, then durably records its exact generation,
refs, reason and operation UUID in `source_retention_releases`. Retries preserve
that request after socket uncertainty. Only a matching confirmed release records
`released_at` and the response; import receipts remain historical audit evidence.
The server never deletes aq-git store paths. Reconstructible cache eviction stays
with aq-git: `repo.cache-evict` is worker-only and explicitly refuses central
retained pools. This sweep therefore invokes `retention.release`, leaving object
reclamation to aq-git's lifecycle rather than attempting central cache eviction.
