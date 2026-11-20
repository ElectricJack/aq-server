# aq-graph deployment canary

`aq.graph-deployment` is a project-scoped Host11 playbook for the
`aq-graph-deploy-canary` development environment. Its manual entry takes a full
merged aq-graph commit SHA and the expected desired generation, starting at
zero. The canary builds each revision from its Git archive on the first-parent
history of the aq-graph `origin/main` ref.

## Run the canary

Use a Linux host with Python 3.11+, the repository's Rust toolchain and a
local Docker daemon with Buildx and OCI-layout support in `docker image save`.
Have the `postgres:17-bookworm` image available locally.

```sh
python3 scripts/test-graph-deployment.py ../../aq-graph \
  aa2590e5b5dc7e79f6a38b28f77f8c3ee91e9a54 \
  5cd9b73e4f54bf06908cd6a8a3a74cdd7fc42a2b
```

The script uses a fresh instance suffix, Docker network, loopback ports and
PostgreSQL credentials. It builds both revisions with aq-graph's `ci-image`
profile and emits the provenance receipts. It then seals the included draft
against the actual Host11 catalog with the installed extension, grants the
extension to its disposable project and admits the playbook on the embedded
AQ driver. All databases in the test belong to the instance. The script
removes its containers, network, build tags and temporary files on exit.

The test checks that deployment waits for Graph readiness, survives AQ owner
restart after dispatch, rejects an unknown source and a stale generation,
and leaves the running container in place when archive verification fails.
It then retries the same build and verifies that a delayed older submission
leaves the newer healthy container in place.

## Opt in and reconcile

The package uses the existing [exec plugin contract](../../docs/specs/0018-plugins.md).
Install the `plugins/graph_deploy` package in the isolated host's configured
plugin directory. Its executable needs Python 3.11+ on the AQ host. The grant
requires only `ext.graph_deploy`. Enable the intended project with a private
`plugins/graph_deploy/bindings.toml` in the isolated host vault.

```toml
[projects."<project UUID>"]
environment = "aq-graph-deploy-canary"
directory = "/absolute/path/to/instance"
```

The instance directory contains `environment.json` with `environment`,
`instance` (a fresh 12-digit hex suffix) and `project_id`. It also contains
the scratch database connection in `graph.env` and the builder's
`provenance/<revision>.json` and untagged OCI image archive
`provenance/<revision>.tar`. Use the resource names and ownership label from
the canary script. Keep the instance directory private and persistent across
AQ and controller restarts. The plugin writes deployment records and
idempotency receipts to its SQLite database there.

Seal the source and draft in `playbook/` against the host's catalog with the
extension present, then install and activate `aq.graph-deployment` for the
canary project. The canary test shows the compiler workspace and binding.
Run the controller on the local Docker host with
`plugins/graph_deploy/bin/graph-deploy tick <instance directory>` at an
interval below the 15-second observation freshness window. The AQ plugin
process only records intents and reads observations. The controller needs
Docker access and uses a fresh CLI configuration under the instance
directory.

The builder is the trust root for this proof. Its receipt binds the Git
commit and source archive checksum to the OCI manifest, image config and
image archive checksums. Before replacing a container, the controller
verifies the archive, manifest and config digests and the image's embedded
source and revision. It imports the untagged image and starts by immutable
config digest. A healthy result includes the corresponding manifest digest
only after Docker observes that config running and reports the image's
Graph readiness check healthy.

An older generation stays superseded. Re-admitting the same revision
recovers its operation and pinned build. An owner can retry the current
failed generation by submitting the same revision with that generation as
the expected generation. The next controller tick verifies the pinned
archive again and reconciles against the existing provider container.
Retain the SQLite state, archive and config together when recovering a
disposable instance. This proof is a local, single-instance development
rollout with a local build trust root. Production promotion, registry
attestation and rollback policy require separate qualification.

## Manual canary result

The disposable `aq-graph-deploy-canary-2bb205f51956` passed through the
embedded Host11 driver on November 19, 2026. Graph returned HTTP success from
`/health/ready` for both generations.

| Generation | Merged revision | Observed OCI manifest digest |
| --- | --- | --- |
| 1 | `aa2590e5b5dc7e79f6a38b28f77f8c3ee91e9a54` | `sha256:42a22d884b5d70688fbb23089a9ed98de8138f82f3196c1e8851818192d07a64` |
| 2 | `5cd9b73e4f54bf06908cd6a8a3a74cdd7fc42a2b` | `sha256:cdbde4023ea003c852cec48ac671d2e5dedf3ac357b843d42c5ad48d5ec4d6a6` |
