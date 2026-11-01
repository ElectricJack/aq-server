# AQ-17 taskless pure central selection

Approved scope A starts at composed `5d445a5`. No owner/SQL migration, HTTP,
registry configuration, credentials, HostPlan, permit, Run, dispatch or runtime
capability is added. AQ-17 stays in progress. AQ-18/AQ-20 task foundations stay
complete; the new adapter does not turn their task snapshots into central facts.

Before extracting any selector code, capture actual canonical outputs and exact
errors on untouched selector0.1.3. Publish selector0.1.4 for the new API; keep the
0.1.3 archive/checkers and all task wire shapes unchanged. The shared private
core owns candidate ordering, availability/cooldown/observation precedence,
model/reasoning/native-limit compatibility, credential matching and fallback.
Task wrappers retain task validation, accounting, retry/replacement rules and
receipt construction. Central wrappers never construct TaskLineage,
ProjectSnapshot, BudgetSnapshot or a made-up execution generation.

## Bounded API and ownership

`aq_selection::central::select_central(actual_catalog, &CentralSelectionInputs)`
returns an outcome with explicit accounting-unavailable and authority-none states,
traces and, when eligible, a separate generated canonical selection artifact.
`verify_central_selection` recomputes it from the actual catalog and original
bounded inputs. It proves data derivation only. The 43ac bundle remains immutable,
unresolved composition data; the selection references its actual composition and
startup artifacts instead of editing them.

The input contains:

- A real central actor/conversation/revision and reservation UUID/revision/batch,
  with original message UUID/cursor/envelope digest references. No message bodies
  are copied outside the original verified composition request.
- Actual canonical reservation LocalPolicy bytes plus their content-derived ref,
  actor/policy revisions; it is independently required enabled. Composition policy
  and compatibility retain their distinct generated43ac references. A separate
  actual central selection policy document pins all three and the reservation,
  exact catalog/agent/intelligence/execution Sources and per-turn ceiling policy.
  None substitutes for or overrides another policy domain.
- One or more actual eligible43ac bundles, each independently verified against the
  supplied actual catalog; same original subject/mail and all policy pins required.
- An actual nonsecret registry snapshot/ref/revision, authorized credential
  definitions and operator observations; a separately identified local central
  installation/incarnation snapshot with exact harness/CLI/adapter/environment/
  qualification/model-reasoning/native-limit/credential-readiness metadata.
  Structural validity is not proof the owner admitted those facts. Worker
  inventory and worker-only credential preparation never qualify a central host.
- An explicit original wall-clock expiry, current sample time and finite per-turn
  context/output/wall/memory/CPU/process ceilings. Native capabilities are required
  as declared; source/model names never infer enforcement. No token estimation.

Initial policy supports one metadata-selection admission, with no execution retry
or replacement. Existing intelligence fallback, observation, output and required
capability rules remain exact. A typed initial-only central attempt policy states
that task max_attempts/max_lineage_elapsed are not central accounting; unsupported
retry/cumulative requirements refuse instead of being interpreted as fresh zeros.
Any monetary ceiling, complete/hard-accounting requirement or task-cost capability
refuses in central-v1. Unknown cost requires explicit bounded permission; spend,
usage tokens and estimates remain unavailable. Shared ModelSettings can have only
`spending_budget=None`, never a task-cumulative basis relabelled central.

The choice pins actual provider/model/native reasoning, credential profile and
revision/account/endpoint, registry snapshot, profile Sources, harness/CLI/adapter/
environment/qualification, composition/startup and original finite expiry. No
provider key/path/private fingerprint appears in inputs or artifacts. Input and
output encoding is bounded with strict fields and duplicate-key rejection.
Context compatibility uses actual rendered startup bytes including two-LF joins,
not the task compiler's raw text total. All fact and observation expiries shorten
the selected metadata expiry; no acknowledgement creates a new duration.

## Qualification and deferred work

Tests cover the untouched task corpus, existing selection/feedback fixtures,
actual43ac compile/verify/select/verify, exact policy/ref/body/batch cross-binding,
unsupported model/reasoning/capability/limits, fallback/availability/account scope,
worker observations excluded, stale snapshots, cost/retry refusal, byte/field/
duplicate/tamper bounds and deterministic canonical output. No provider/SQL/host.
Use existing dependency closure only. Root coordinates short pure Cargo leases.

Deferred B is an authenticated internal central preparation/artifact-retention
owner using full current Owner identity, actual schema37 source/reservation policy,
active catalog/registry/explicit central placement and genuine local installation
facts. It compiles outside SQL then rechecks all original facts before retaining
immutable bounded artifacts and an exact operation receipt. Replay is not renewed
validity; cancel/policy/registry changes fence use. A future separately reviewed
schema must preserve per-actor operation identity and cancellation headroom; no
migration number is allocated here. Do not overload task intelligence_decisions.
Deferred C owns subject-neutral HostPlan/permit/material, physical startup/stop,
reply authority and lifetime accounting. API-07 runtime stays unavailable through
A and B; no production eligibility follows from a pure selected model.
