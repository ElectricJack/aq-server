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
`cargo run -p aq-profiles --example export > crates/aq-profiles/tests/golden/implementer.json`.
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

## Roles and profiles (spec 0014)

The `roles` module is the replacement for the catalog above
([spec 0014](../../docs/specs/0014-roles-and-profiles.md)). It is built
alongside the catalog until its consumers have moved over. It is pure: no
database, HTTP or file access.

An agent is described by three documents, each one Markdown file:

| Document | Says | Parsed by |
|---|---|---|
| Role | What kind of work the agent does: its prompt, its AQ operations and the execution profile it runs under. | `roles::parse_role` |
| Intelligence profile | How capable a model the work needs: ordered candidates, and optionally its own values for the selection policy. | `roles::parse_intelligence` |
| Execution profile | Where and how the harness runs: harnesses, environment, confinement, credentials and limits. | `roles::parse_execution` |

`roles::compile` parses and validates all documents together. It returns a
`Snapshot`, or every problem it found, each naming its document and field. A
snapshot is immutable and is identified by the SHA-256 digest of its canonical
JSON bytes. Entries have no version numbers; an attempt records the snapshot
digest it launched with. The shipped defaults are the files under
[`defaults/`](defaults), returned by `roles::default_documents`.

`to_markdown` on each type writes the document back, and the parser reads that
text as the same value.

### Grammar common to the three documents

A document is frontmatter followed by sections.

- **Frontmatter.** The first line is `---`. Each following line is
  `key: value` until a closing `---` line. Blank lines are ignored. A key that
  the document kind does not define, or a key given twice, is refused.
- **Values.** Text runs to the end of the line, with outer spaces removed.
  Text that is empty, has outer spaces or starts with `"` or `[` is written as
  a JSON string (`name: "[draft] role"`). A list is `[a, b, c]`; its items are
  bare words of letters, digits and `. _ @ / : + -`. A flag is `true` or
  `false`. A number is decimal digits.
- **Sections.** A line that starts with `## ` opens a section. Each kind
  defines its headings. Any other `## ` heading, a heading given twice, or
  text before the first heading is refused. A `## ` line inside a fenced code
  block (```` ``` ```` or `~~~`) is content, not a heading.
- **Whitespace.** Lines may end in LF or CRLF. Spaces around values, list
  items and table cells, and blank lines around and between sections and list
  items, do not change the parsed value or the snapshot digest. Inside a
  role's prompt text, everything except leading and trailing blank space is
  content.

`id` is a lowercase letter followed by up to 62 lowercase letters, digits or
`-`. Ids are unique within a kind. `description` is optional in every kind.

### Role

```markdown
---
id: developer
name: Developer
description: Makes the change a task asks for and reports evidence.
execution: sandbox
operations: [task.read, knowledge.read, source.write]
enabled: true
---
## Role
You implement the change your task describes, in the workspace you are given.

## Rules
- Change only what the task asks for.
- Run focused checks and report the exact commands and their outcomes.
```

| Part | Rule |
|---|---|
| `execution` | Required. The id of an execution profile in the same set. |
| `operations` | Required list; may be empty. Each is an agent-grant operation (`aq_agent_grants::op::ALL`) or `source.write`. No scope: `roles::effective_permissions` gives each the task's project (`project:<id>`), for `source.*` the project's repository (`repository:<id>`), or for `wait.*` the attempt itself (`attempt:self`), and keeps it only when the project's ceilings and the caller both hold it. |
| `enabled` | Optional, `true` when absent. At least one role in a set must be enabled. |
| `## Role` | Required and not empty. |
| `## Rules` | Optional. |

Use `###` for headings inside a section.

### Intelligence profile

```markdown
---
id: standard-high
name: Standard, high
description: Ordinary work that touches several parts or needs careful checking.
---
## Candidates
| provider | model | reasoning | harnesses |
|---|---|---|---|
| anthropic | claude-sonnet-5-5 | high | claude |
| openai | gpt-6 | high | codex |
```

A profile is its candidates and nothing else by default.

`## Candidates` is a table with exactly this header row, a rule row, and one
row per candidate in preference order. `harnesses` is a comma-separated list.
`reasoning` is the effort itself and must be one the named harness accepts
(`roles::harness_supports`):

| Harness | Reasoning values |
|---|---|
| `claude` | `default`, `off`, `low`, `medium`, `high`, `xhigh`, `max` |
| `codex`, `opencode` | `default`, `low`, `medium`, `high` |
| `gemini` | `default` |

The table is the worker's: it refuses any other value by name.

`default` is the harness's own setting, so a profile can name a model on any
of the four harnesses. The `claude` harness takes provider `anthropic` and
`codex` takes `openai`. A profile cannot list the same provider, model and
reasoning twice. A candidate has no id to write; `Candidate::derived_id` is
`provider:model:reasoning`.

#### Selection policy

The selection policy has one set of defaults for all profiles
(`roles::SelectionDefaults`). Settings holds it and the caller passes it to
`roles::compile` and `roles::validate`; it is part of the snapshot.
`SelectionDefaults::default()` is what a new installation starts with:

| Key | Default |
|---|---|
| `max_attempts` | `2` |
| `fallback_on` | `[missing_credentials, unavailable, unsupported, stale_unknown, policy_denied]` |
| `max_lineage_elapsed_ms` | `3600000` |
| `observation_max_age_ms` | `30000` |
| `allow_unknown_availability` | `false` |
| `unknown_cost` | `allow_bounded` (or `deny`) |
| `require_complete_accounting` | `false` |
| `require_hard_cost_enforcement` | `false` |
| `max_cost_microunits`, `currency` | none; they go together |

A profile overrides single keys under an optional `## Selection` heading, a
list of `- key: value` items:

```markdown
## Selection
- max_attempts: 3
- fallback_on: [unavailable]
```

Every key is optional, and a key the table does not list is refused.
`max_cost_microunits: none` and `currency: none` state "no cost limit" where
the defaults have one. A compiled profile keeps only the keys whose value
differs from the defaults, and `to_markdown` writes only those; with none, it
writes no `## Selection` section. The rules of `SelectionPolicy::validate`
apply to the defaults and to each profile's defaults-plus-overrides.

A profile does not state the output token limit or the capabilities a worker
must have. Both come from the role's execution profile: its
`max_output_tokens` limit, and `Execution::required_capabilities`, which is
`aq.profiled-launch`, `aq.terminal`, `aq.harness.startup-context`,
`aq.harness.stop`, and `aq.harness.container-confinement` when `confinement`
is `container` or `container-permitted`.

`Snapshot::resolve(role, intelligence)` returns what that pair launches with:
the role, its execution profile, the profile's candidates limited to the
harnesses the execution profile may use, and the effective `SelectionPolicy`.

### Execution profile

```markdown
---
id: sandbox
name: Sandbox
description: A node-owned container running the Claude harness, 15 minutes per attempt.
harnesses: [claude]
environment: aq-ms1-docker-local-claude
confinement: container
---
## Credentials
- claude: owner-claude-subscription

## Limits
- wall_time_ms: 900000
- memory_bytes: 4294967296
- cpu_millis: 2000
- process_limit: 512
- startup_context_bytes: 65536
- max_output_tokens: 32000
```

| Part | Rule |
|---|---|
| `harnesses` | Required, not empty. Each is `claude`, `codex`, `gemini` or `opencode`. |
| `environment` | Required. The sandbox environment. |
| `confinement` | `container` (always in a container), `container-permitted`, `native` (a container is denied) or `native-required`. `Confinement::execution_confinement` gives the worker's `ExecutionConfinement`. |
| `## Credentials` | One `- harness: credential profile` item for each harness listed, and for no other. |
| `## Limits` | All six keys are required and positive. `startup_context_bytes` and `max_output_tokens` are each at most 1,048,576. |

An execution profile has no permissions and names no project.

### Size limits

`roles::compile` and `roles::validate` take a `DocumentLimits` from the
caller: bytes per document, bytes for the whole set, documents per kind, and
bytes for a name and a description. `DocumentLimits::default()` holds the
catalog's values (65,536; 1,048,576; 128; 128; 2,000).

### Startup text

`roles::assemble_startup` builds what the agent is told, in the order of
spec 0014 §6.1: platform text, the role's `## Role`, its `## Rules`, the
project's notes for the role, the repository's notes, the task, the reporting
contract. Absent or blank notes and absent rules are left out. The parts are
joined by one blank line (`utf8_double_newline_v1`). Text over the byte limit
the caller passes is refused; nothing is dropped to make it fit.
