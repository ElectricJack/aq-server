# AQ-17 taskless central authority core: design and implementation delta

This first increment is a pure, independently tested model. It performs no SQL,
HTTP, credential access, process launch, tmux operation, mailbox write or migration.
Structurally valid data and reducer decisions are NOT execution authority. Runtime
execution, dispatch and reply capabilities remain false. A future authenticated
owner must verify referenced facts, fence current policy/session/permit, and CAS
its state before any physical effect.

A global supervisor owns a real logical actor and conversation. Its subject is
`central_conversation`, never a fabricated project, task, graph workspace or worker.
The existing `SessionIdentity::Central` is reused and its generation must equal the
outer binding generation. The actor policy and catalog/agent/intelligence/execution,
composition, selection, host plan, adapter/environment and startup references are
exact immutable artifact pins. They are references, not resolved permissions or
proof that any installation is qualified. The same actor/conversation identity
survives a new physical session; a generation increments exactly once per successor.

One plan is one bounded turn over one immutable, ordered mailbox batch (1..16
message references). It contains no message body or credentials. Batch identity,
original payload digests and cursor references join to AQ-11's actual records in a
future owner. The canonical plan digest binds every input. A separate dispatch and
host-input operation identity is bound to that exact session, plan and batch.
Dispatch intent and start intent are recorded together before physical startup;
Host Running is not proof of stdin delivery, native readiness or a reply.

The original wall deadline is fixed at admission on one Linux CLOCK_BOOTTIME
machine boot. Permits last at most 120 seconds and cannot pass the original wall
deadline. Renewals require a still-live prior permit, the next sequence and a
strictly later expiry; exact live retries do not extend anything. Foreign boot,
clock regression, changed binding or same-sequence changes refuse. Cancellation,
expiry and uncertain host stop cannot be undone by a late renewal/Running report.
Expiry is observed before an event is evaluated, so a refused renewal still leaves
an explicitly stopping model state.

Cancellation before start records no physical start. After committed start intent,
cancellation only requests stop. Stopped-report data requires an exact launch,
guardian and stopped-writer receipt reference; the core cannot verify that receipt.
Unknown stop prohibits a successor. Physical stop does not prove mail was never
handed off: uncertain dispatch stays unknown until explicit exact original-input
receipt reconciliation. Unknown or known-handed-off inputs cannot automatically
appear in a successor batch. A stopped session may have a disjoint next batch while
its earlier uncertain association remains retained. Known not-handed-off evidence
may permit the original immutable references on a successor; changed payloads under
an old message UUID always refuse. State replacement never erases the previous run.

The public neutral adapter seam is CentralPlan + CentralBinding + PermitData +
DispatchAssociation and artifact references to original local evidence. No host or
harness implementation is imported. The future host bridge must consume a new
versioned subject-neutral HostPlan; it must not weaken the frozen v1 LaunchManifest
validator or create placeholder task IDs. AQ-18 composition and AQ-20 selection
also need an explicit central subject/budget adapter through their shared core,
not a copied profile compiler or model selector. Common harness mapping should be
extracted from aq-worker into its own package; remote offer/material/journal joins
stay in worker wrappers. Current Codex exec is one immutable startup turn with
input disabled, not qualified interactive message injection or native resume.

API-07 remains human-only queued mail with an unavailable/null runtime and
not_attempted delivery. A future version must publish authenticated central reply
and dispatch associations before those fields change. SQL ownership, central tool
credentials, private scratch/material ownership, native transport/named-image/
capsule qualification and actual host startup are later increments. Media owns
migration 36; this core introduces no state-format change.

Grounding at daemon `918559f` and worker `baea71f`:
`vendor/aq-session-protocol-0.1.0/src/lib.rs` (SessionIdentity/LaunchManifest),
`crates/aq-profiles/src/compile.rs` (CompileRequest/FrozenComposition),
`crates/aq-selection/src/domain.rs` (TaskLineage/ExecutionTarget),
`crates/aq-admission/src/lib.rs` (MaterialLease),
`crates/aq-admission/src/execution_spec.rs` (ProfiledExecutionSpec), and worker
`crates/aq-session-host/src/lib.rs` (ExecutionPermit/LaunchAuthority) plus
`crates/aq-worker/src/native/adapter/codex.rs` (harness mapping).
V1 reference inventory pins 9a9b8e8. Additional inspected v1 paths at that pin:
`src/commands/supervisor_authority.py`, `src/commands/conversation_commands.py`,
`src/database/queries/conversation_queries.py`, `src/sessions/supervisor_health.py`.
Preserve exact live-instance reply authority, stable global addressing while
absent, launch-scoped observations and retained retry budgets. Replace the old
named-session/local SQL authority and transcript heuristics at v2 boundaries.

Implementation: publish versioned closed data schemas/examples; implement bounded
validation/canonical pins and the pure reducer; test adversarial identity/generation,
renewal/deadline/clock, cancellation/uncertain stop, exact handoff reconciliation,
successor non-replay, schema/example stability and always-unavailable capabilities.
Tests require no daemon, database, tmux, sibling worker checkout or provider.

Successor admission additionally requires a bounded `MailHistoryFence` covering
every exact candidate reference at the actor's current generation. Each reference
must match both its original mailbox UUID and cursor; aliasing either identity to
different mail refuses. The future
owner must query ALL retained dispatch generations under its actor-generation
fence, returning explicit unseen/retryable/held classification plus an immutable
source snapshot reference. A missing row is not evidence of no prior dispatch.
The pure helper checks complete candidate coverage, source subject/generation and
known original associations; held inputs refuse even when they predate the
immediate predecessor. The owner authenticates the query/snapshot and CAS, not
this core. The run never grows an unbounded history list.

The history query covers authoritative message UUID AND mailbox-cursor identity,
and both candidate operation UUIDs across dispatch/host-input roles and every
retained generation. Those operation IDs share one per-actor lifetime namespace.
Explicit query coverage is required; absence of an entry refuses. Initial model
construction only admits generation 1; later generations require the retained
successor fence (restoration uses validated snapshots). These are data consistency
rules; the future owner still authenticates initial creation and all history.

The versioned public envelope is `Document {document, data}` with strict known
variants and unknown-field rejection; `decode` additionally validates cross-field
constraints not expressible in JSON Schema. Canonical digests use compact UTF-8
JSON with recursively sorted object keys and lowercase unprefixed SHA-256. The
complete encoded document is limited to 65,536 bytes. Mail history is a bounded
query result (1..16 exact candidates and exactly two operation candidates), not a
copy of all prior runs. A history row carries the authoritative original MailRef
when either UUID or cursor matches retained mail, allowing the helper to reject
changed aliases. The future indexed query must not label such an alias unseen.
Each operation row explicitly states unused or retains its original generation,
role and association digest; used candidates refuse irrespective of old role.
The future owner must retain every dispatched association or a semantically
complete durable hold index; garbage collection cannot silently erase holds.

`Run::new` builds only an initial generation-1 model. `successor` requires the
previous run to have a reported stopped-writer reference (or never-started
cancellation), exact generation increment and new session/operation identities,
plus the complete history fence. `decode(State)` restores a structurally
consistent snapshot without proving its provenance. Callers must never treat
restoration as a fresh grant. Transition `Applied`/`Replayed` describes a model
change only; `Refused` may still carry the conservative stop/unknown transition
caused by elapsed time. Owners must retain that returned state rather than
reusing their old unexpired snapshot.

Validation command: `cargo test -p aq-central-authority --offline`; export check:
`cargo run -p aq-central-authority --example export --offline --
crates/aq-central-authority/artifacts/schema.json --check`. No workspace-wide
server fixture or provider is needed. The optional export `--examples` mode
publishes typed example documents under the same check path.

Focused qualification for this increment: 16 pure tests pass (0.02 seconds test
execution), all-target strict Clippy passes, both deterministic artifact export
checks pass, and all eight public documents independently validate against the
Draft 2020-12 schema. This evidence covers model consistency only. It does not
qualify durable dispatch, physical stop, reply authority or a central runtime.
