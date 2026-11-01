# Central profile/startup data, version 1

This is a pure Rust data contract in `aq-profiles` 0.1.2, not an HTTP, daemon,
agent, credential or host interface. It has no relation to activating a live
execution capability. Task CompileRequest, CompositionDraft and FrozenComposition
schemas 1/2 remain unchanged and are checked against the pre-refactor 0.1.1 corpus.

`compile_central(&ValidatedCatalog, &CentralCompileRequest)` uses the same private
DAG, permission, dependency/optional omission and budget algorithm as the task
compiler. `decode_request` accepts strict raw request JSON; the schema's
`Document {document,data}` envelope only groups versioned documentation artifacts.
The input is capped at the existing strict JSON parser's 1 MiB encoded limit.
Unknown/duplicate fields, fractional integers, unsupported subject/model or
credential fields refuse. Call compile/verify for cross-field and byte bounds;
JSON Schema alone does not establish eligibility or provenance.

The new subject is `central_conversation` with nonnil actor/conversation UUIDs,
positive conversation revision and an exact reference to actual supplied policy
bytes. It has no project, task, workspace, attempt, execution generation or
physical session. The policy kind `central_profile_composition` is a new data
schema, not schema-37 reservation policy or a current execution grant. It binds
actor/conversation/revision, enabled state, exact catalog/recipe and compatibility
digest, an explicit actor permission ceiling and context-byte ceiling. The
caller must separately obtain and authenticate its actual owned/current policy.

Compatibility kind is explicitly `central_conversation`. The sidecar pins the
exact actual catalog and agent/intelligence/execution Source identities and a
duplicate-free set covering every expanded module, including optional packs.
Missing/stale/wrong/extra identities refuse. Declaration order does not control
instruction order. A recipe named supervisor is not implicitly compatible.
Existing Markdown remains literal; this engine never guesses a template's intent
or expands task/shell substitutions. Compatibility is an explicit reviewed data
declaration, not a caller signature or runtime qualification.

Installation, policy/actor, execution, caller and actual catalog-execution
permission ceilings are intersected exactly with registered-tool metadata. No
wildcards, prose grants, path inference or capability inheritance are introduced.
Unknown required adapters/tools make compilation ineligible. Optional omission
removes dependent instructions and their grants through the shared algorithm.
The only allowed override remains an agent-declared narrowing context byte limit;
central override provenance must match actual supplied context/policy/compatibility
bytes. External context cannot impersonate reserved catalog/compiler namespaces.

The central wrapper generates required policy, conversation and immutable
untrusted-mail frames around actual supplied data. A mail turn has 1..16 ordered,
distinct message UUID/cursor/original-envelope-digest references and complete
UTF-8 bodies. Each body is at most 16,384 bytes, total raw bodies at most 65,536,
and body hashes must match. The envelope digest is not substituted with a body
hash. A future owner must verify those references against the original MQ
receipt and current reservation; this pure compiler cannot authenticate MQ.
Required mail is never silently truncated, dropped or acknowledged to fit.

External context bodies retain the existing 64 KiB limit. Generated required
frames may occupy up to 1 MiB, with aggregate external/generated input bounded
by the shared 1 MiB context-input cap. Effective startup ceiling is the minimum
of actual execution, actor policy and permitted narrowing override. Every retained
startup instruction after the first costs its text plus two LF bytes. Required
bytes reserve first; optional priority/dependency removal uses that same charge.
On-demand text is explicitly retained as reference material, excluded from
startup bytes and not labelled as zero-token work.

Successful compilation returns exact instruction/permission/source data and:

- Actual canonical request, policy and compatibility artifact bytes/references.
- Compiler descriptor pinned to actual new package/source bytes; renderer
  descriptor pinned to its actual deterministic joining implementation.
- Actual generated mail frame and rendered UTF-8 startup bytes/references.
- Canonical composition bytes binding those refs plus actual validated catalog,
  profile/module identities, source ordering, omissions and byte accounting.

Artifact IDs are content addressed under `aq.central-profile.<kind>/<sha256>`,
version 1. Each SHA-256 is lowercase unprefixed hex over the actual UTF-8 bytes.
The catalog ref points at actual ValidatedCatalog canonical bytes under
`aq.profiles.catalog/<sha256>`. Profile/module Source hashes come from that actual
validated catalog's immutable definition identities; no prompts are copied into
another catalog. Artifact bytes are bounded to 8 MiB each. Verification uses a
bounded 16 MiB typed bundle comparison, never an unbounded serialization of
supplied data. The renderer is deterministic two-LF UTF-8 joining, with no shell,
provider-home scan, template execution or installation probe.

`verify_central(catalog,bundle)` checks generated byte hashes, decodes the exact
retained request and recomputes the complete result under the actual current
compiler, rejecting rehashed-but-changed instructions/permissions/refs. It proves
deterministic data derivation from those inputs, not authenticity, current
catalog/policy ownership, artifact retention, installed renderer qualification,
execution authority or mailbox delivery. A later owner must fence those facts.

Selection is always `unresolved`; authority is `none`; selected_model,
token_count and cost are null. No default model, fake selector receipt,
availability observation, token estimate or task-cumulative cost budget is
invented. Actual selection must get its own central subject/budget adapter using
the selector core. Then a real central HostPlan/permit/material owner can join
these bytes to shared HarnessSettings/ModelSettings and the pure Codex mapper.
The daemon never depends on all of aq-worker. There is no DB/schema-38, driver,
HTTP route, provider call or live runtime change in this increment.

Normative schema/examples are data vectors, not live qualification. Pure tests
include the captured task corpus, actual catalog compatibility and provenance,
UTF-8/separator bounds, dependent-grant omission, generated byte/source pins and
tamper rejection. New package archives get new identities; all old archives and
their old checkers remain unchanged.

Regenerate/check the normative vectors with
`cargo run -p aq-profiles --example export-central -- protocol/central-profile/v1 [--check]`.
The catalog example is actual validated fixture data, not a daemon installation.
The raw request is the `data` value of `request.example.json`; the outer envelope
is documentation only. `compilation.example.json` contains the full typed result,
including exact generated UTF-8 artifact bytes. SHA references deliberately change
when actual compiler source bytes or package identity change.
