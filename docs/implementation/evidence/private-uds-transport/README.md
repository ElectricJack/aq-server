# Isolated actual PostgreSQL UDS transport proof

The reviewed helper 371c7b0 created exactly one new private PG 17.11 container from
the cached immutable image. No image pull, published port, TCP listener or
forwarding proxy was used. Actual [transport result](result.json) verifies kernel
peer UID/GID 1000, real 0600 socket under 0700 host directory, Unix SQL connection,
local peer auth, empty listen_addresses and system identifier 7703296609124593685.
A second independent inspect returned the same physical cluster identity.

`SO_PEERCRED` returned PID 0 across the local namespace boundary. The verified UID
and GID do not constitute a process/PID attestation, and no such claim is made.
AQ itself checks socket ownership/inode/path as documented in the
[provisioning/recipe](../../private-uds-optional-runtime-proof.md); the extra kernel
credential and system-ID observations are operator proof, not new server checks.

The exact labelled container and mount/image/policy identity passed helper
inspection. Read-only numeric account files let the actual Postgres process run
as 1000; no existing container/account/security policy or socket permission was
changed. The existing 55432 service retained its exact ID, empty mount list and
host port. Canonical 4188 retained 5c assets with Decisions/Playbooks unavailable.
No canonical data was restored or copied and no AQ/engine secrets were created.

Both generated private connection files point at the same new Unix endpoint,
port and fresh database. Metadata is checked 0600/UID 1000/single-link; their
contents are not copied into evidence. Neither the actual AQ SQLx pool nor the
playbook engine was started yet. This qualifies the physical transport prerequisite,
not the full optional-owner runtime or canonical availability.

[Qualification/pins](qualification.json) retain the exact helper hash, image,
container ID, file metadata and limits. The private instance remains only for the
next reviewed structural-fixture/actual-owner phase; its data files are not
release artifacts. `stop --root <recorded private root>` validates the exact
labelled resource before removing that container and retains private files for
review. No global prune, canonical rollback or database restore is involved.
