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
