# AQ compositional profiles (AQ-18)

`aq-profiles` is a pure Rust catalog validator/compiler and a trusted persistence
interface. It has no database, provider, worker process, shell, installer hooks or
model calls. `aq-session-protocol =0.1.0` and `aq-session-api =0.1.0` remain pinned
public contracts. The daemon adapter owns authenticated central activation and
current tenant/project/caller authority; passing a compiler request is no grant.

Run the actual CLI without a daemon:

```
cargo run -p aq-profiles --bin aq-profile -- profile validate --catalog crates/aq-profiles/examples/catalog
cargo run -p aq-profiles --bin aq-profile -- profile explain --catalog CATALOG --request REQUEST.json --graph-access-profile-digest SHA256
```

This binary currently supplies only `aq-profile profile validate` and `aq-profile profile explain`.
Validate optionally accepts the same request/digest arguments as explain.
`validate --artifact-out FILE` exports canonical immutable artifact bytes for the
central activation API; export alone never activates a catalog. Exit 0
means catalog/composition validation succeeded, 2 means malformed/overflowing
input and 3 means a structurally valid but ineligible composition. Output separates
`catalog_valid`, `composition_valid` and `selection_pending`. Intelligence remains
unselected: nested CON-02 explanation `valid=false` means *launch resolution is
incomplete*, even when composition is valid. AQ-20 must choose an eligible concrete
candidate using authenticated fresh observations, credentials and budget policy;
this crate never pretends a catalog candidate is an available model.

The example catalog is a configuration template, not a production launch default.
Its four recipes share immutable engineering/reporting and small AQ work/knowledge/
source packs across Codex, Claude Code, Gemini and OpenCode. Intelligence candidates
are explicitly `configuration-required`; no production model, CLI version,
credential, adapter readiness or provider availability is fabricated. Configure
new definition versions before deployment. Tests supply deterministic capability
observations for composition only. The optional `dependency-wait@1` pack requests
only scoped register/checkpoint/show/ack plus end-turn/bootstrap instructions; its
required `m5.wait-adapter` remains unavailable in M4 and no baseline recipe depends
on it. It conveys no operator recovery or cancellation permission.

Catalog format: one nonsecret `catalog.toml` using `schema_version=1`, arrays of
`modules`, `agents`, `intelligence`, `execution`, and explicit `id@version` refs.
Every record denies unknown fields. Modules own relative Markdown instruction
paths, purpose, startup/on-demand delivery, required/optional classification,
explicit dependencies, operation/scope requirements and required adapters. Behavior
modules cannot request grants or depend on capability packs. Agent recipes list
behavior/pack references and intelligence/execution references; no inheritance or
per-model/harness role prompt exists. Required content cannot be on-demand.

Module expansion is dependency-first DFS in declared root/dependency order,
identity-deduplicated. A dependency cycle, missing reference, wrong-kind root or
multiple versions of one logical module in a composition refuses validation.
All definitions/recipes are checked, including unselected ones. Required roots
transitively require their entire dependency closure; that closure must be startup
content. An unavailable optional dependency omits dependent optional modules too.
Budget omission also removes optional dependents and their effective operations,
so instruction removal cannot leave a dependent capability silently active.
Behavior modules
appear before capability startup instructions. Prompt order is platform, role and
behaviors, pack startup instructions, project conventions, exact assignment,
labelled context. External context references bind exact UTF-8 bytes by digest;
text is never interpreted as policy. Sources/revisions accompany every instruction.

The only implemented override is an agent-explicitly-allowed
`context_limit_bytes`, requiring source and reason and only narrowing the execution
ceiling. Unknown/protected/duplicate overrides refuse. There is no last-writer-wins
permission merge. Effective operations must exactly match requested logical
operation/scope pairs, supported registered tools and every installation, project,
execution, caller and catalog-execution ceiling. Required denial makes composition
ineligible and exposes the denied requirement. Optional denial is an explained
omission. No wildcard matching or physical path guessing is performed. A required
unavailable module/adapter is visible and cannot produce a frozen composition.

Required startup byte totals are reserved first; overflow refuses with offending
source identities. Optional startup inputs are omitted in ascending
`omission_priority`, preserving input order for ties, until the remainder fits.
Module optional content has priority zero. On-demand references do not occupy the
startup budget and still require current scoped retrieval at delivery time. No
unqualified token estimate is emitted. The frozen pre-selection composition and
its digest are not worker `LaunchManifest`s and cannot authorize execution.

Canonical artifacts preserve exact Markdown bytes and canonical typed metadata.
JSON object keys sort recursively; arrays retain their meaningful declared order.
SHA256 lowercase hex names both catalog artifacts and namespaced definition
identities (`module:engineering@1`, `agent:implementer@1`, etc.). TOML whitespace
outside values is not semantic content; Markdown whitespace is content. Changing
any canonical definition bytes under an activated ID/version conflicts permanently,
including after rollback or artifact pruning. Advance every changed definition's
version, including recipes/dependants whose references change. Friendly references
resolve to exact item digests in the immutable artifact; no per-poll directory scan
is needed after activation.

`CatalogRepository` is the central adapter boundary: immutable publish/get,
revisioned state and compare-exchange. `activate` computes a full validated CAS
update; invalid edits and stale/conflicting activation leave the previous active
catalog intact. Publishing before losing CAS can leave an unreferenced immutable
artifact, never a partially active catalog. `retain` pins exact attempt/history
references, `release` retires those references, and `can_remove` protects active or
retained artifacts. Rollback activates a previously validated artifact for future
work and never rewrites retained attempts. Durable identity tombstones must survive
artifact pruning. The in-memory implementation is a deterministic test adapter,
not fleet authority or an alternative editable registry. Production repository
implementations must serialize CAS/retention checks and authorize these operations;
retention is driven by real attempts and retained records, never client claims.

Bounds: 1 MiB catalog TOML plus loaded Markdown aggregate, 64 KiB each Markdown or
external context body, 1 MiB aggregate external context, 128 items per definition/
requirement list and composed instruction plan, dependency depth 32, 16 overrides,
1 MiB execution startup context, 2 MiB serialized immutable catalog artifact,
100,000 retained reference entries and 100,000 permanent identity tombstones per registry adapter. References are bounded
nonempty text; IDs are ASCII alphanumeric/underscore/hyphen up to 96 bytes with
positive versions. Input paths reject absolute/parent paths and all symlink
components. Descriptor-relative no-follow opens keep traversal beneath the opened
root across renames; nonblocking regular-file checks refuse FIFOs/devices, and
cap-plus-one reads stay bounded if a file grows after its metadata check. The loader reads only the explicitly named catalog and its Markdown;
there is no directory-wide prompt/provider scan. Definition bodies are nonsecret;
credentials have references only. Arbitrary authored text cannot be proven secret
free; callers must scope inspection and avoid logging private inputs.

Tests: `cargo test -p aq-profiles`; strict lint:
`cargo clippy -p aq-profiles --all-targets -- -D warnings`.
The golden composition is regenerated explicitly using
`cargo run -p aq-profiles --example export-aq-profiles > crates/aq-profiles/tests/golden/implementer.json`.
Golden data and all tests require no database, sibling checkout or live provider.

Owner-only daemon inspection routes `/api/profiles/validate` and
`/api/profiles/explain` accept `artifact_json` (canonical exported artifact string),
optional `composition` (CompileRequest), and accompanying
`graph_access_profile_digest`. Explain requires a composition snapshot. Their typed
response includes `authority_admission=false` and never activates a catalog or
creates execution authority. Submitted capability/ceiling/context snapshots are
inspection inputs; they are not authenticated observations for launch admission.
These owner tools are separate from the future CON-02 `/api/v1/profiles/...`
context-resolving handlers that will obtain current AQ-19/AQ-20 policy themselves.
Both use existing owner authentication and CSRF rules. Inspection is bounded to
8 MiB HTTP bodies, 2 MiB artifact strings and 2 MiB serialized composition inputs.
