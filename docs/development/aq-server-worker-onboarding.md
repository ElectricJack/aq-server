# aq-server on the existing aq-ms1 desktop worker

The owner requested aq-server execution on the same worker as scratch. The
rollout adds a project-specific module, execution profile and agent profile;
permissions name only aq-server's project and repository. Existing intelligence
rows and the scratch permission plan remain unchanged. The other five AQ
projects stay paused.

Initial source is the retained real main commit
`f5e5e54e52f441a0140a6e3924eaf147a5ef9c91`, with whitespace-only
`.gitattributes` preserved. The project uses immutable source selection, so a
later source update requires a fresh sync, retained checkpoint and revisioned
project source-policy update. This avoids relying on a stale named-ref
observation. No remote publication target is allowed; completion retains a
local result for review (`delivery: not_required`).

The host rollout lives in the private directory
`~/.local/share/aq-ms1/aq-server-onboarding/`; its backups are in
`backup-before-aq-server-activation/`. It updates only the ms1 node-service
instance and its scoped Git binary, plus isolated central Git/server images.
The owner's `aq-worker@local.service`, Supabase and shared installed binaries
are outside this rollout. The two existing enrolled node identities are kept.

The Claude CLI, sandbox driver, image, client, credential and prompt renderer
are unchanged. New versioned qualification metadata explicitly scopes the two
installed permission plans. Counting/capacity evidence is reused with its
actual observed/expiration timestamps; no fresh measurement or extended
validity is claimed. Old definition revisions remain retained in the database;
the active startup configuration selects only the current revision of each.

## Slow preparation

A real repository export can exceed the control HTTP request's five-second
budget. Every nonzero-capacity poll now starts or joins one bounded background
assembler per enrolled node (four globally). It returns an empty page after
750 ms if preparation is still in progress. The assembler keeps its permit
until completion, and normal retained-offer redelivery serves the next poll.
Repeated polls cannot restart or duplicate that node's assembly. All original
source, Graph freshness, selection, admission, publication and grant checks
remain in place. Cancellation and zero-capacity polls stay prompt.

The regression test pauses a real fixture's source observation, confirms two
polls return without duplicate work, resumes the same assembler, and reads
its retained offer on the next poll. The existing slow-cancellation test
checks that the cancellation path still permits background completion.

For rollback, stop aq-server through its normal project control, preserving
queued tasks and history. Restore the saved ms1 instance service/native config,
renderer and operator configuration as a coordinated set, and select the prior
isolated images. A database restore is not required and would overwrite newer
owner work. Do not roll back the owner's separate local worker or global pools.

Source API1 advertises an optional 1 MiB range capability. Legacy clients
continue using 256 KiB; new clients fall back when the header is absent. The
server keeps every existing per-read authorization and retention fence. Six
source-route regressions cover advertised bounds, exact data, refused oversized
ranges, changed credentials, symlinks and expired authority. The ms1 preparation
window is configured to the protocol-supported 120000 ms while the requested
until-ready setup lifecycle is implemented; that window is not the intended
user-facing setup timeout.
