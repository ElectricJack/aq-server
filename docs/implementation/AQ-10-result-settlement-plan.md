# AQ-10 native stop, result disposition and graph settlement

Status: Stage A implemented as a state21 candidate, under qualification, 2026-10-31.
Stages B/C remain planned and do not follow automatically from a stopped receipt.
Root owns this lane; AQ-08 owns native assignment/preparation/grants, and aq-worker
owns actual journal/host/source producers. The shared
[execution contract](../architecture/0002-execution-and-recovery.md) is normative.
No receipt reference or worker success text independently establishes an outcome.

## Existing owners

The common task claim spans fixture and native attempts. Adopted native authority
uses `profiled_authority_hot`; sealed manifests, selections and launch bindings
remain immutable. AQ-08's Reserved/Prepared/Started journal is monotonic. Started
binds the actual host guardian, tmux pane and configuration to the logical
pre-grant owner. The source-retention service owns real aq-git retention receipts,
AQ-07 owns authorized graph mutation intents/recovery, and artifact retention owns
actual nonsecret bytes. Reuse those owners and the common AuthorityView.

The existing native ResultReport and WorkerStopEvidence DTOs provide bounded
claims/references, not a verifier for a kernel stopped proof. The legacy fixture
result path remains unchanged and cannot be used to assert native completion.

## Stage A: actual stopped-host evidence

Publish additive `aq-admission`0.1.4 types for a bounded stopped report and immutable
historical acknowledgment. Preserve all0.1.3/public session bytes. The request
contains the existing WorkerStopEvidence plus the actual serialized host
StoppedProof, its canonical digest, exact reservation, runtime and qualification.
Its stop operation/journal/binding/grant must agree. Only Stopped with the host's
ECHILD evidence is admissible; Unknown, mere exit codes, pane disappearance and a
receipt digest without proof are not accepted as stopped.

Initial scope is a host with a previously recorded Started owner. Match the
physical guardian nonce/process/machine boot/PID namespace, complete LaunchIntent
and exact pane identity to the stored Started request. Stop observation time, permit sequence and
reaped counters cannot regress. An unchanged permit sequence preserves its exact
deadline; a newer sequence may legitimately shorten the execution deadline. A physically new guardian cannot close an
old launch. A grant that never started or a host lost before Started needs its
separately qualified no-start/recovery producer; do not invent its proof here.

Recovery admission requires current authenticated machine credentials, exact
node/boot/attempt and the originally admitted native runtime/qualification. It
must not require an unexpired execution grant or current permission to launch:
expired/cancelled work still needs to report stopped evidence. Removing a runtime
from the current launch allowlist cannot manufacture stop, but also must not
prevent an otherwise valid original-owner stop report. A revoked machine or
changed boot without supported recovery authority refuses and preserves the hold.

Transaction order: current node lock, actual admitted runtime identity, common
attempt and hot row, immutable Started receipt, then owner journal. Exact
historical retry returns the original acknowledgment without changing any row or
clock. Changed operation reuse, a competing stop report or mismatched proof
conflicts. New admission advances the journal from Started to Stopped and records
one immutable stopped receipt in the same transaction as terminal authority and
the stopped projection. SQL guards independently bind the receipt, journal and
hot transition; missing either side rolls back. Sample recording time after the
last lock, but never compare a worker boot clock with central wall time.

Migration021 implements this owner. Record `Finished` execution authority
with known stopped evidence; this is not task success or accepted result. Keep
the original grant/deadlines and cancellation identity unchanged. Do not set
retention/effects complete, clear `holds_task`, release disk, or emit graph
completion from this transition. A stopped receipt reference is derived from the
actual canonical acknowledgment bytes, not a caller-chosen string.

Verify with pure public-contract fixtures and independent canonical vectors,
real disposable PostgreSQL/router tests for exact replay/conflicts, cancellation/
expiry recovery, wrong physical/logical owner, missing Started, stale credentials,
configuration retirement, atomic rollback and direct SQL projection refusal.
Actual synthetic tmux StoppedProof JSON must round-trip through the published
mirror before claiming the host/daemon producer seam. This does not qualify a
paid provider or substitute for the future worker journal integration.

## Stage B: native result and evidence admission

Keep an immutable report identity/request digest and explicit supersession.
Authenticate the current worker recovery context and exact native binding; read
real stop receipts from Stage A rather than trusting the report's references.
Load actual retained source receipts and required result artifacts from their
owners, including project/repository/attempt/base binding and retention horizon.
Pending agent/domain effects must be reconciled through AQ-07's existing operation
records. An omitted pending-effect list cannot establish quiescence.

Validate the pinned outcome contract through a finite server-owned validator.
Unknown contracts refuse automatic eligibility. Agent conclusions and process
status remain claims/observations; they cannot replace executed checks, accepted
review or a revision-bound human decision required by policy. Any additional
outcome-artifact wire fields need additive versioned contracts, not a silent
reinterpretation of ResultReport.conclusion.

Serialize cancellation and result eligibility under current authority, recheck
current project/task/input revisions, and retain stale/cancelled reports without
automatically settling the task. Expose retained, eligible, rejected/stale and
needs-reconciliation dispositions separately. Exact replay reads the original
record; a new evaluation of changed evidence uses a new explicit operation.

## Stage C: claim release and graph completion

Release is a producer-owned transaction joining actual stopped proof, sufficient
central retention and actual effect-quiescence evidence. Insert the verified
release record and clear the common claim atomically. Completeness of projected
receipt fields is necessary but not sufficient. Resource release and substantive
task completion remain distinct operations.

For a policy-eligible result, persist an authorized expected-version completion
intent through the existing AQ-07 delivery boundary. The actual graph receipt is
the completion fact. Lost replies reconcile the same intent; version conflicts
remain conflicts and cannot trigger read-new-version/overwrite. Publish the
confirmed domain event in the same AQ transaction that records the graph receipt,
so playbooks see one logical completion through durable outbox replay.

Bound reports, evidence lists, lookup pages and retries. Bulk evidence/source reads
must not consume reserved execution-control capacity. Retain unresolved work
rather than pruning it to satisfy quota. Before Stage B/C coding, record the exact
new report/validator schemas, outcome-policy dispatch and source/effect queries
against the AQ-08 producer interfaces; these later stages are not established by
the initial stopped owner.

## Product acceptance

GATE-04 still requires a real qualified first harness to receive a real graph
task, edit confined source, stop, hand off retained evidence and reach confirmed
version-checked completion. Add lost-result and lost-graph-response recovery,
cancellation-before/after-eligibility races, stale revisions, uncertain writers
and source survival after stopped-worker data retirement. Stop-owner qualification
alone cannot close AQ-10 or GATE-04.

## Stage B contract decision: expected outcome, before result admission

The next additive public package is `aq-admission`0.1.5. Preserve0.1.4 archives
and every prior wire/schema. Add `outcome::ExpectedOutcomeDefinition`, a retained
nonsecret definition created before the execution spec. It has schema1 and one
initial finite kind, `source_change_reviewed`; unrecognized kinds/versions cannot
establish eligibility. This is an expectation, not a result or approval.

Fields are project ID/revision, existing TaskRevision (workspace, task, graph CAS
version and content revision), source repository/binding generation/base object
and retained base artifact, authorized task observation/context artifacts, up to
64 additional input artifact references, and1–32 acceptance criteria. Each criterion
has a unique <=512-byte ID and <=8192-byte UTF8 text drawn from the authorized task
projection. Preserve text and provenance; reject oversized input rather than
silently dropping requirements. The entire canonical definition is <=65536 bytes.
There is no attempt ID, manifest hash or execution-spec hash in this definition,
so the composition graph remains acyclic.

A pinned validation-policy ArtifactRef names the exact centrally selected policy.
The finite review requirement is either `human_owner` or `policy_actor`; both
require an explicit revision-bound decision from an authorized central owner.
There is no `none`, agent-self-approval or process-success shortcut. Zero to32
uniquely named required checks pin exact validator artifacts. These are evidence
requirements, not executable strings, shell commands, plugin discovery or proof
that a check ran. Initial native assignment must choose a configured policy and
supported validator set; this contract does not fabricate their implementations.

`validate_against(ProfiledExecutionSpec)` checks the exact project/task/source/base,
observation/context references and the content-derived outcome ArtifactRef in the
spec. The producer retains the canonical bytes through the existing Outcome
artifact owner and pins that same reference in the manifest. Result admission
loads the original definition and actual retained evidence; it cannot replace
requirements with fields supplied in a final worker report.

Review/check evidence will bind the original outcome digest, report ID/digest,
exact retained resulting source object/artifact and current expected task/input
revisions. AQ validates the decision's authenticated owner and immutable evidence
record, then rechecks applicability under current authority. Agent text or a
reference to an unknown validator is retained only as a claim. Cancellation,
changed task/input revisions, unresolved effects and unknown policy/validators
prevent automatic eligibility. New evidence evaluation uses a new operation;
historical report acknowledgments never silently change disposition.

This slice publishes only the shared expectation contract and pure validation.
The bounded immutable report ledger, finite evidence-policy evaluator, explicit
review/check owners, source/effect-quiescence queries and graph completion delivery
are subsequent Stage B/C work. No outcome definition or public DTO authorizes
release, accepts a result or performs a graph mutation.

## Stage B1: immutable report recording, before eligibility evaluation

Use the existing bounded `transport::ResultReport` unchanged. Add a separate
`aq-admission`0.1.6 result-recording response and schema, preserving all earlier
archives. A report is <=65536 UTF8 JSON bytes, has one stable report ID and operation
ID, and references the exact Stage A stop evidence. Canonical bytes determine its
digest. Initial scope is the post-Started/source-change path; unknown/no-start stop
claims require their own recovery owner. Reported process outcome and conclusion
remain worker claims, independent of the host wait status and graph task state.

The immutable receipt identifies the original operation/report/binding, report
digest, pinned expected-outcome artifact, actual stopped receipt and recording
time. Its only disposition is `recorded`. It acknowledges retention of the
report, not acceptance of its conclusions, source sufficiency, effect quiescence,
resource release or task completion. Later immutable evaluation operations have
separate IDs and dispositions; repeated report POSTs never reevaluate evidence or
silently turn an old receipt into an acceptance. Source receipt IDs and pending
operation IDs in the report are claims to resolve through their existing owners,
not a new database copy of source/effect truth.

Daemon ownership (migration024, after native022 and playbook023; no placeholder
migrations): authenticate and lock current node, then exact attempt/hot identity,
original admitted runtime and original Started/Stopped records. Resolve the
immutable manifest's outcome reference and typed definition through the existing
inline artifact owner, binding it to the original execution spec. Do not require
live execution authority or current launch eligibility to record stopped work.
Under the same attempt lock, require a first report with no predecessor or a
correction that supersedes the current report head. A report ID is immutable;
changed operation/report reuse or competing corrections conflict. Bound each
attempt to64 retained reports and4MiB of canonical report bytes; refuse new work
at capacity and retain existing exact replays. Do not overwrite prior reports.

Insert the canonical report and receipt and advance its report head atomically.
The report table has immutable-body guards, unique operation/report identities,
a source stop-owner foreign key and binding/digest checks. Admission does not
change hot authority, task claims, source retention, effects or graph state.
Cancelled and now-stale attempts may retain reports; their later eligibility is
independently evaluated. Heavy evidence resolution runs outside reserved control
capacity. The initial ledger does not claim that recording a reference proves
that its source/check/review evidence exists.

Before Stage C permits release/re-registration, add an authenticated historical
receipt lookup: a current machine credential may read its own exact old receipt
without gaining old-boot execution authority. The current Stage A POST checks the
original admitted boot/runtime before replay; after future incarnation replacement
that POST can refuse an old acknowledgment. Preserve that conservative behavior
until the explicit historical-recovery path is qualified; do not call a lookup a
new stop report or fresh authority grant.

Required ledger tests cover unchanged public ResultReport bytes, exact replay,
changed IDs/bodies, correction races, quota/replay, actual Stop/Outcome owners,
wrong physical/logical binding, expired/cancelled recording, credential rotation
while blocked, atomic rollback, and unchanged held task/source/effect/graph state.
Only the recorded-receipt pure contract can be qualified before migration024 and
actual owner integration. Stage B evaluation and Stage C release remain open.
