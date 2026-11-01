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
machine boot. Permits last at most120 seconds and cannot pass the original wall
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
migration36; this core introduces no state-format change.

Grounding at daemon918559f and workerbaea71f: session protocol SessionIdentity and
LaunchManifest; aq-profiles CompileRequest/FrozenComposition; aq-selection
TaskLineage/ExecutionTarget; aq-admission MaterialLease/ProfiledExecutionSpec;
aq-session-host ExecutionPermit/LaunchAuthority; worker native/adapter/codex.rs.
V1 reference inventory pins9a9b8e8. Additional inspected v1 paths at that pin:
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
