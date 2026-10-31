# AQ21 migration012 and shared session inspection checkpoint

This checkpoint contains schema and the shared read-authorization helper only.
It does not install terminal routes, enable a relay capability, create a terminal,
issue input/controller permission, launch a process or qualify a renderer/provider.
The full AQ21 core/protection/runtime work stays in a separate worktree.

Base: `d177695` (includes AQ08 migration010 and AQ24 migration011). Migration012
is registered with server/readable state format12 and Docker state-format12;
old servers refuse that new format. The usual updater backup/rollback contract
applies. No production database or release/product pin was changed.

`session_inspection_acl` binds the SHA256 canonical **complete SessionIdentity**
and its stored typed identity to an authenticated current user, revision and
expiry. `session_access::require_inspect_locked` runs after the common node/attempt
authority locks, then rechecks the current user role and exact ACL under SQL locks.
It samples expiry after those locks. Owner inspection is explicit; Viewer grants
are per exact session. This one bounded read capability covers watch/peek/logs/
usage; it grants neither terminal control nor graph-profile authority. AQ22 can
consume this helper without any terminal configuration/keyring dependency.

Control permissions are separate in `terminal_control_acl`; exclusive expiring
controller leases remain future runtime state. Other tables hold exact profiled
terminal registrations, encrypted ledger/outbox bytes, immutable metadata-only
operation replies, input delivery metadata and scoped subscription projections.
Encryption fields have version/key-ID/nonce/ciphertext limits. The schema cannot
itself attest process ownership or a successful input injection; typed authority,
AEAD, replay state and transaction adapters remain mandatory before runtime use.

`terminal_sessions UNIQUE(attempt_id)` and immutable terminal ID/epoch/session/
node/boot/generation/source seals intentionally prevent swapping a host terminal
under the same execution grant. A host terminal replacement requires a new
execution attempt. Updating current authority revision for the **same** surviving
exact host/identity must revoke old controller authority and preserve input
uncertainty; it cannot launch or substitute another tmux instance. Worker host/
bridge ownership evidence is still a separate qualification requirement.

Validation uses fresh disposable PostgreSQL17.11 image
`sha256:266d3c257594e195b309ae9d1805e54c788f1d7b375f56bf6c367aac6c917e7b`,
2CPUs/1GiB with fsync, synchronous_commit and full_page_writes on. Four actual PG
tests pass: ordered migration/version/table creation; current Owner/Viewer role,
revocation/expiry/deletion; all remote identity tuple fields and corrupt ACL-body
refusal; expiry following an observed `pg_stat_activity` ACL lock wait. The existing
state-format/latest-migration unit test, focused strict Clippy and formatting pass.

The initial smoke run correctly caught a missing MIGRATIONS include (the file
existed but version11 remained active); it was fixed before qualification, and
its failed log is retained. No per-request schema creation/fallback was added.
Synthetic native fixtures remain Offered and qualify no runnable/provider grant.
The separate worktree is `/home/jkern/dev/agent-q-aq21-migration`; its evidence is
retained under `target/aq21-migration-evidence` and copied to the immutable bundle
listed in the companion JSON. Shared Cargo build cache was used, but compilation
source paths/manifests are this migration-only worktree, not unfinished runtime.

The reviewed source checkpoint is `bb57616`, cherry-picked to main as `7769508`.
Tracked qualification logs and the exact disposable-database script are in
`AQ-21-migration12-evidence/`. The recorded version12 assertion describes this
checkpoint; later additive migrations should assert the current server format.
