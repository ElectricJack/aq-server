# Proposed canonical empty-Playbooks cutover after actual UDS staging

Status: **executed and accepted** for the local canonical preview; see the
[actual final cutover record](evidence/canonical-uds-empty-owner-live/README.md).
The remaining text preserves the reviewed proposal before execution. The successful
[actual canonical-data clone stage](evidence/canonical-uds-empty-owner-stage/README.md)
was available on4218/4219 and is now retired. Before execution, canonical4188/4189 used its original state35
backend/database/config. No final snapshot, target creation, canonical config
change, stop or promotion is implied by this proposal.

Use the current union binary
`/tmp/aq-composed-central-empty-511ee99/aq-server`, SHA256
`17698c625675b7a994b0a56bbd37737becbf039380093ae9dd32fe1bdcd27b25`,
source511ee995ac55dd6345590732705b5a3bfde5130d/default37/no test-support, and
unchanged dashboard624707540cc6566158c3721e4733f69934c8a1670c4ffb972afb7d469fb8e663.
Both have now served actual protected APIs on the real UDS stage. No build or new
PostgreSQL server/dependency is required.

## Actual authority that must survive

Keep the actual Owner id/issuer/subject/role/email, enabled project1/workspace3
binding revision2, Graph/auth/source/terminal config semantics, protected-value
AQ_PRIVATE_VALUE_KEY_K1, original rows and sequences. Preserve the existing one
execution attempt and its contradictory retained projections exactly:
execution_attempts offered/holds_task=true; hot authority finished. Its historical
phase grant, started receipt and expired material lease remain retained.
Disposition is **RETAIN UNRESOLVED UNCHANGED**, with no release, repair,
reconciliation, new attempt, changed expiry or authority extension. No physical
worker-stop claim is made from this state.

Source-backed startup preserves that disposition: omitted profiled_execution
configuration makes policies::install an immediate no-op and expiry::run exit.
Main starts no other automatic attempt scheduler/reissuer. Empty Playbooks has no
grants/artifacts/effects; its review/event scans join its own namespace. Actual
startup/restart and the later511ee99 union keep all critical node/attempt/material/
phase/receipt tables logically fingerprint-identical. Terminal startup changes
runtime_id only, preserving its ledger7/authority/observed/output timestamps.

The latest coordinated process scan finds only canonical PID874715 with the old
DB pointer. Root already retired the owned older4196/4197 preview. At the captured
preflight there are no current node heartbeats/worker TCP connections, unexpired
material leases, terminal pending/controller/outbox, notification deliveries or
source transfers. Recheck these facts at final quiescence; do not replace a
nonzero retained hold with an all-zero readiness assertion.

## Proposed final lifecycle

1. Root freezes human/worker mutations for the local cutover and coordinates all
   writers. Capture exact canonical PID/start identity, config/environment pins,
   original binary/assets/DB pointer and actual authority/connection preflight.
   Stop only the owned canonical process through graceful SIGTERM. Confirm4188/
   4189 closed, no remaining same-DB writer process and no original DB SQL clients
   except the operator's read-only backup transaction. Retire/stop the stage when
   review is complete so it does not remain an accidental second development UI.
2. Take a **fresh final consistent** read-only snapshot/backup from the old source
   database after quiescence. The earlier stage copy is a point-in-time observation
   and must not be promoted as if synchronized. Retain backup0600/checksum and
   source DB unchanged for rollback. Keep original unresolved holds and receipts.
3. Restore into another NEW uniquely named database in the same owned private UDS
   PostgreSQL instance. Preserve UTF8/en_US.utf8 collation/ctype/libc2.36, roles,
   database owner/privileges and sequences as qualified. Refuse an existing target;
   do not overwrite the stage or original/inert DB. Before startup compare all
   original logical row fingerprints, sequences, actual principal/project and
   held-claim/grant/receipt state against the frozen final snapshot.
4. Prepare a separate final private vault/connection files. Preserve original
   server4188/4189 and public_origin http://localhost:4188 and every original
   non-Playbooks config value, plus private protected-value environment. Both core
   and engine point to this ONE actual UDS database. Configure a fresh canonical
   namespace for the genuine Owner/project/workspace with deployments=[] and new
   one-time bridge/engine keys. Keep its identity distinct from the disposable
   stage namespace; no artifact, grant, activation, recipient or expiry is seeded.
   Record exact new config/key/provenance pins before service start.
5. Root starts the qualified511ee99 default backend with624 assets against the
   new target. Verify AQ37/component4, actual Unix core/bridge/engine clients,
   principal identity and new authorization context, current empty Definitions/
   Activations and loaded-definition capability false. Compare original data
   with only the already qualified migration/runtime metadata differences; retain
   the offered held claim exactly. Run bounded GET-only catalog, Graph/Overview/
   Work/Knowledge/Agents/Supervisor/Results/Settings checks. Resume user/worker
   access only after the canonical pointer and single writer set are recorded.

No current grant or timestamp is silently refreshed. A PostgreSQL restore changes
xmin/MVCC and bootstrap authorizationRevision; preserve actual principal identity,
accept a new authorization context and keep old ephemeral request pins fenced.
The final namespace is newly configured real authority, not the staged namespace
presented as a continuation of its incarnation counter across another database.

## Rollback boundary

Before target user writes, stop the new daemon, retain its DB/evidence, and return
to the unchanged old DB/config/environment/binary/assets. Confirm single-writer
ownership again. The old database was never migrated in place and remains compatible
with its old binary. After any target user writes, returning to the old snapshot
would discard them: stop/freeze writers and preserve both databases for a reviewed
forward repair or reconciliation/export. Do not silently switch between divergent
copies. Root owns final lifecycle authorization and that boundary decision.
