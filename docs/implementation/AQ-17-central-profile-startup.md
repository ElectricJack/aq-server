# AQ-17 central profile and startup composition bridge

This pure increment starts at daemon `ba65fbe`, after qualified reservation 37.
It adds no schema 38, HTTP, driver, model call, reservation consumption, Run,
execution permit, credentials or host startup. The previous reservation owner
stays explicitly unavailable for profile/plan binding. The output supplies actual
generated composition/startup artifacts for a later authenticated owner, rather
than invented task identities or opaque placeholder profile pins.

## Grounding and preservation

`crates/aq-profiles/src/compile.rs::compile_internal` already owns dependency
expansion, required closure, module ordering/deduplication, exact permission
intersection, availability omissions, narrowing overrides, instruction byte
budgets and dependent capability removal. Its CON02 ProfileRequest validation and
final CompositionDraft/FrozenComposition/explanation wrappers are task-specific.
`graph_policy.rs` additionally requires a real task graph scope. Neither can be
called with a pretend project/assignment on behalf of the global supervisor.

`catalog.rs::ValidatedCatalog` already verifies exact metadata plus Markdown,
versioned namespaced definition identities and the entire dependency DAG. Existing
Catalog schema 1 has no subject-compatibility field. Markdown is literal text;
there is no existing template interpolation engine to infer task requirements.

The old `aq-profiles` 0.1.1 and `aq-selection` 0.1.2 package archives and their
checkers remain immutable. Publish profiles 0.1.2; selection 0.1.3 changes only its
exact profiles dependency. Update the daemon's exact dependency pins. Old task
CompileRequest, CompositionDraft, FrozenComposition schema 1/2 and public errors
remain byte/behavior compatible. Capture a real task-output/error corpus under
untouched compiler 0.1.1 BEFORE refactoring, then compare against the new shared
core. Keep historical catalog/composition goldens unchanged. Legitimate new
central compiler/renderer identities use the actual new version/source bytes.

## One shared compiler core

Extract the existing algorithm into a private subject-neutral request/result
module. Its inputs are the actual ValidatedCatalog, exact selected recipe key,
four explicit permission ceilings, registered-tool/adapter metadata, harness and
environment, mandatory context slots, labelled extra context and allowed
narrowing overrides. Its result contains actual catalog Source identities,
instruction order, effective/requested/denied operations, omissions, required and
optional byte counts, reference material and blocking reasons. Do not copy the
algorithm into another compiler or fabricate a CON02 ProfileRequest.

The old task wrapper retains its exact request validation and error order and
maps platform, project and assignment into the shared core. It retains the
existing raw-text budget accounting and its original wire/explanation fields.
The central wrapper supplies platform plus actor policy, conversation and one
immutable mail turn. It has no project/task/workspace/attempt/session fields and
never calls graph-policy sealing. Shared omission/dependency/permission behavior
remains one implementation.

For central startup, account for the actual deterministic separator bytes as
well as text. The first mandatory platform instruction cannot be dropped; every
later startup instruction costs its text plus two LF separator bytes. Required
content reserves its full charge first. Optional removal uses the same shared
algorithm with that charge mode. On-demand content has no startup charge; it is
retained as an explicit reference and cannot silently become a tool grant.

## New central data API

A separate version-1 central request carries:

- A taskless actor/conversation subject with positive conversation revision and
  an exact reference to supplied actual composition-policy document bytes.
- A closed central policy data document with actor/conversation/revision,
  explicit actor permission ceiling and context-byte limit. This is actual
  nonsecret configuration data, not schema-37 reservation policy repurposed as
  execution authority. Its generated digest/reference must match the subject.
- Exact catalog digest and versioned agent recipe, plus a catalog-bound central
  compatibility sidecar listing exact Source identities for the agent,
  intelligence, execution and complete expanded module closure.
- Installation/execution/caller permission ceilings, bounded tool/adapter
  metadata, harness/environment and actual platform/policy/conversation/mail
  context bytes, with digests and provenance. Optional context and overrides
  remain explicit and bounded.

The compatibility sidecar contains no copied prompts. It defaults to denial for
legacy or missing declarations; a role named supervisor is not sufficient.
Missing, changed, wrong-version, duplicate or task-only/unlisted definition refs
refuse, including reachable optional packs. A declaration covers exact content
and catalog, not a friendly-name wildcard. It is an explicit compatibility
statement, not NLP analysis, caller authentication or runtime qualification.

External context cannot impersonate reserved catalog namespaces. Role/module
source identities come only from the actual validated catalog. Policy,
conversation and mail contexts must match their taskless subject and actual
provided bytes. Each supplied mail reference remains ordered and immutable;
required bodies cannot be truncated, omitted or silently replaced to fit. The
future owner must independently verify them against the original MQ receipt and
current reservation before supplying them. This pure layer cannot authenticate a
mailbox, a policy owner or a current grant.

The public operation is `compile_central(&ValidatedCatalog,
&CentralCompileRequest) -> CentralCompilation`. It returns an ineligible result
with explicit reasons for unsupported required tools/adapters/ceilings, or a
pre-selection central composition with real catalog sources, actual immutable
instruction data, context plan and actual rendered startup bytes. Shape,
provenance, compatibility and required-byte overflow are validation failures.
No constructor accepts a precomputed caller digest as proof of generated bytes.

## Genuine artifact bytes and renderer

Canonical composition bytes receive a content-addressed ArtifactRef whose digest
is computed from those exact bytes. The catalog/profile/module refs are derived
from ValidatedCatalog canonical bytes/identities. The policy and compatibility
refs are computed from their actual supplied canonical documents. Startup ref
hashes the exact emitted UTF-8 bytes, with deterministic two-LF joins and no shell
or template evaluation. Bound each input, aggregate context, output and encoded
artifact; reject NUL/invalid UTF-8 and required content overflow.

The renderer descriptor records a new explicit algorithm/version and the digest
of the actual renderer implementation source bytes (and actual crate version
where relevant). Its ref is generated from that real descriptor, not borrowed
from a worker qualification or assigned a dummy digest. This is reproducible
renderer identity, not evidence that an installation, model tokenizer or harness
qualified it. Changing renderer bytes changes its pin. Output artifacts are
returned data, not a claim that storage retained or activated them.

Model selection is deliberately unresolved in this first slice. No candidate is
chosen, no placeholder provider/default model is manufactured, and token/cost
counts are unavailable rather than zero. Catalog intelligence and policy metadata
remain exactly pinned for the later selector. Current `aq-selection` has a
Central target but still requires TaskLineage/ProjectSnapshot/BudgetSnapshot;
its future central adapter must reuse the selector's policy core with a genuine
central budget subject. This increment does not copy availability/fallback logic
or invent a selection receipt. Shared ModelSettings currently has task-cumulative
spending-budget bases; never reinterpret those as central accounting.

The eventual mapper seam remains shared HarnessSettings/ModelSettings plus
`terminal_input_enabled=false`, consumed by pure `aq-codex-mapper` (worker
`f0b6b76`) after authenticated selection/HostPlan assembly. Do not depend on the
whole worker or duplicate its Codex settings/argv mapping. Current Codex 0.166
mapping supports OpenAI low/medium/high and refuses spending budgets; its CLI
recipe does not enforce the selected output cap. Qualified scoped transport,
central token/cost policy, subject-neutral HostPlan, physical permit, credential
privacy and stop/reply authority remain independent gates.

## Tests and implementation sequence

Commit this design before compiler edits. Capture untouched task 0.1.1 canonical
outputs and error strings for legacy compilation, draft/native sealing,
permissions, narrowing/denied overrides, dependency/optional budget omissions,
unsupported adapters and provenance/identity refusals. Release the short capture
build lease, then refactor and implement the bounded pure central adapter.

Use actual validated catalog/Markdown data and an explicit central compatibility
sidecar. Test exact task-corpus/golden preservation; shared fragments emitted
once; default-deny compatibility and stale/missing refs; actor/conversation/
policy/provenance mismatch; all permission ceilings; optional dependent grant
removal; required UTF-8/separator-byte overflow; deterministic ordering/rendering;
exact generated artifact/source digests and tamper refusal; unavailable selection/
token/cost/runtime claims; unknown fields, cardinality and byte limits. No test
requires PostgreSQL, daemon, sibling worker, model/provider, credentials or launch.

Qualify the new profiles/selection package closure independently while preserving
all historical archives and old checker behavior. Root coordinates narrow Cargo
pins and any later integration. There is no claim of central execution readiness
until real selection, profile/HostPlan ownership, artifact retention and physical
authority are separately implemented and qualified.

## Completed first pure increment

Source `43ac07b` implements this compile/render boundary in profiles 0.1.2.
Selector 0.1.3 only follows that dependency; its selection algorithm is unchanged.
The compiler source itself has a genuine new identity, recorded by generated
compiler/renderer artifacts rather than presented as the old task compiler.
Task wire and errors match 21 actual canonical cases captured on untouched
0.1.1 before refactoring, in addition to the existing task/native goldens.

The independently extracted five-package closure passes all 60 tests, strict
Clippy and formatting offline. New package payloads byte-match source `43ac07b`;
all old archives/checkers remain unchanged. The frozen schema, actual catalog/
request/result examples and package inventory are in
[`protocol/central-profile/v1`](../../protocol/central-profile/v1/README.md).
Reproducible qualification details and exact archive hashes are in
[`evidence/aq17-central-profile-43ac07b`](evidence/aq17-central-profile-43ac07b/qualification.json).

This completes pure composition and rendering only. A future central selection
adapter, owner-retained artifacts and exact subject-neutral HostPlan/permit/
material/reply associations are still required before any production execution.
The reservation owner and API-07 continue to expose no supervisor runtime.
