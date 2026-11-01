# AQ media state 35→36 promotion rehearsal and rollback draft

**Review only. No canonical cutover performed.** The copied-database migration
and backup-based format rollback passed. Baseline `55b1dc6` is superseded by
the qualified owner-only capacity fix `b455eb0`; frozen migration36 is unchanged.
Twenty-one actual owner/HTTP cases passed in24.61s, strict Clippy in22.10s and
the default binary build in38.25s. Its exact source pin, SHA and regression are in
the [capacity qualification](evidence/api11-media-capacity/qualification.json).
The private4214 rehearsal now runs that fixed binary on its original36
database/root/storage binding; human data, schema and sequences remain exact.
Parent review still precedes any canonical change. The subsequent owner-only
result/node-before-intent fix `4800aab` passed the
[actual lock-order qualification](evidence/api11-media-lock-order/qualification.json).
Its new binary and rehearsal refresh are still required before promotion; the
running private rehearsal remains the earlier capacity-qualified binary.

The [counts/hash summary](evidence/api11-media36-rehearsal/summary.json) contains
no message bodies, user names, credentials or raw conversation/actor IDs. Raw
archives, configuration, bootstrap data and detailed manifests remain private
0600 inside the 0700 artifact directory recorded there. Earlier artifacts remain
untouched. The preserved old executable is owner-only 0500.

## What was rehearsed

Canonical 4188 remained on format35 throughout, with the same process, binary
SHA `8ad0aad8…` and bundle manifest SHA `5c0fcfa4…`. All 109 table row-bag hashes
and sequence states matched before and after the work. Identity comparisons use
the real conversation singleton and existing Owner, with only their SHA256
values published in the summary.

A fresh private custom-format35 dump was exported against the same PostgreSQL
read-only repeatable-read snapshot used for the count/hash manifest. It is
547,211 bytes with SHA256
`85d266f752d07df309336199c8202c8d17bb440bc4dd73685427cbf393fac6d3`.
Restoring into a new disposable database reproduced all 109 table bags, all
sequence values and all 1,919 pre-existing schema object definitions exactly.
The rehearsal restored as the same PostgreSQL owner with no custom relation,
function or default ACLs; ownership/namespace ACL summaries also matched.

The pinned55 binary and af51 bundle then started against that restored35 copy,
performing the actual migration36 and managed-root initialization. It used a
new private0700 root and stable storage UUID. Uploads were disabled. Bootstrap
was disabled for identity creation while the existing dev identity remained
usable. Unrelated terminal, agent-access, profile and playbook runtimes were
omitted, and an isolated maintenance spool paused notification dispatch and
mutations. The Graph service was neither changed nor called.

At36, all 1,919 old schema objects still matched. Of the 109 old table bags,
106 matched exactly. The only differences were the state-format value, one new
migration36 ledger entry and notification dispatcher identity. Old ledger rows,
other meta values and all other notification budget fields matched exactly.
Actor/project/mailbox/message/receipt/history bags and the conversation identity
were preserved. Three media tables were added; they contained one storage root,
one empty installation budget and zero media intents.

The isolated service remains reviewable on4214/4215. Readiness, health and
bootstrap returned200 with the exact candidate source revision; media read was
enabled for Owner and upload was disabled. Canonical has zero AQ28 report rows,
so Results discovery correctly returned an empty typed page. An unknown report's
media route returned generic404 rather than a fabricated successful empty media
page. The known-report positive media proof remains the separate4206 fixture.

The exact old35 executable was also pointed at the copied36 database. It refused
with exit2 and left the entire captured36 state unchanged. A second new database
was restored from the35 backup; the actual old35 server then started successfully
on it and reported35 readiness. The only startup difference was its dispatcher
control identity. The36 rehearsal was restarted with its original private root
and storage UUID. Only rehearsal processes were signalled.

This establishes data/schema preservation and backup-based format rollback. It
does not qualify a full canonical operational configuration or authorize its
enablement. The current copied mail tables are empty; their exact bags and all
other existing data were nevertheless compared.

Private `promotion-config.resumed.draft.toml` and
`promotion-config.quiesced.draft.toml` are available inside the rehearsal artifact
directory. Both preserve every original semantic setting. The resumed draft adds
only media with uploads disabled; the quiesced draft additionally points at a
dedicated maintenance spool. Their SHA values are recorded in the counts/hash
summary. No proposed canonical root/spool directory has been created. At the
approved cutover, create the private maintenance marker before starting the
quiesced configuration and keep ingress/worker writers held through validation.
Switching to the resumed configuration is itself a reviewed resume step.

## Proposed promotion protocol, after the capacity fix is accepted

1. Pin the reviewed fixed **state36** binary, its exact source revision and SHA,
   and the selected reviewed dashboard manifest. Do not substitute an unpinned
   newer main binary that may introduce another state format. Rehearse the fixed
   owner and required canonical configuration checks before authorizing cutover.
   Keep the existing35 binary, config, bundle and key files available unchanged.
2. Quiesce canonical writers and stop the old daemon gracefully. Drain in-flight
   commands, worker mutations and dispatch work. Confirm the current database is
   still35 and capture fresh identity/count/hash checks while writers are held.
   This is the final quiesce point; the earlier rehearsal dump is not the
   production rollback checkpoint.
3. Make a **new uniquely named private35 dump after quiescence**. Preserve all
   ownership/ACL information, record its SHA and verify restoration into a fresh
   disposable database. Keep its private config/key references with the checkpoint.
   Never overwrite an earlier backup or expose a DSN/password in command output.
4. Prepare the reviewed canonical config from the actual existing config, not
   the deliberately minimal rehearsal config. Add only the agreed media settings
   with a new managed0700 root, stable nonnil storage UUID and
   `uploads_enabled=false`. Preserve existing secret/key files and unrelated
   configured owners. Review any expected runtime-owner control-row changes.
5. Start the fixed36 binary against the quiesced canonical database with the
   pinned bundle. It performs the real migration and startup refusal checks.
   Before resuming writes, require36 readiness/source SHA, the exact conversation
   and Owner identity hashes, unchanged human data/history/old schema, the exact
   old migration ledger plus36, and empty media intent/quota state. Validate the
   live configured-root binding and read/upload capabilities. Use read-only
   probes; do not create actors, tasks, projects, reports or test uploads.
6. With no AQ28 reports, retain the honest empty Results response and unknown
   media404. Do not seed a report to manufacture a visible image. Resume writers
   only after the parent reviews all checks. Enabling uploads or a later uploader
   bundle is a separate reviewed step; its fixture does not authorize a canonical
   upload.

## Proposed failure and rollback protocol

**Switching back to the old binary after36 is not rollback.** Its actual refusal
was verified. Do not edit meta, delete ledger36, drop media tables or otherwise
relabel a36 database as35.

If a failure occurs before writes are resumed, stop the new daemon, preserve its
private logs/database/media root for diagnosis, and restore the fresh quiesced35
backup into a clean database. Verify original identity/hash/ledger/schema/sequence
checks before pointing the old binary at that restored database. Restore the old
config, bundle and unchanged key references, check35 readiness and Owner access,
then resume writers. Keep the failed36 database and any media files intact.

Once writes resume, an old backup omits subsequent conversation/mail/task
operations and media receipts. A restore can therefore lose data and cannot
undo notification, Graph or Git effects. Re-quiesce and preserve a new36 forensic
checkpoint; inventory and reconcile post-checkpoint operations before deciding
between forward repair and an explicitly accepted data-restoration plan. Do not
automatically restore Graph or Git as part of an AQ database rollback.
