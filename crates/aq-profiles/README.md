# AQ roles and profiles

`aq-profiles` holds the role, intelligence profile and execution profile
documents of [spec 0014](../../docs/specs/0014-roles-and-profiles.md) and the
snapshot they compile into (module `roles`), the selection policy a launch
runs under, and the graph access policy sealed for a launch. It is pure: no
database, HTTP, provider or model calls. The server stores the documents and
activates snapshots (`aq-role-settings`).

Validate a directory of documents without a server:

```
cargo run -p aq-profiles --bin aq-profile -- validate crates/aq-profiles/defaults
```

The directory holds `roles/*.md`, `intelligence/*.md` and `execution/*.md`,
as [`defaults/`](defaults) does. The documents are compiled together with the
default selection policy and size limits. Exit 0 prints the snapshot digest;
exit 3 prints every problem, each naming its document and field; exit 2 means
the directory could not be read.

## The three documents
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
| `operations` | Required list; may be empty. Each is an agent-grant operation (`aq_agent_grants::op::ALL`) or `source.write`. No scope: `roles::requested_permissions` gives each the task's project (`project:<id>`), for `source.*` the project's repository (`repository:<id>`), or for `wait.*` the attempt itself (`attempt:self`), and `roles::RequestedPermissions::effective` keeps it only when the project's ceilings and the caller both hold it. |
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
| `fallback_on` | `[missing_credentials, unavailable, unsupported, stale_unknown, policy_denied]` (retained for compatibility; queued tasks always consider the other eligible candidates) |
| `observation_max_age_ms` | `30000` |
| `allow_unknown_availability` | `false` |
| `unknown_cost` | `allow_bounded` (or `deny`) |
| `require_complete_accounting` | `false` |
| `require_hard_cost_enforcement` | `false` |
| `max_cost_microunits`, `currency` | none; they go together |
| `max_uncharged_continuations` | `5`: owner- or service-stopped, lost or never-started attempts a task may have before a further one is charged |
| `max_wake_attempts` | `3`: launches a task may have spent for a waiting session to still be woken (defaults only) |
| `max_wake_lineage_elapsed_ms` | `86400000`: time since a task's first offer within which a waiting session is still woken (defaults only) |
| `max_never_granted_offers` | `8`: offers of a task that may expire without ever being granted before the task is held (defaults only) |
| `launch_retry_backoff` | `{"initial_delay_ms": 30000, "max_delay_ms": 300000}` (defaults only) |
| `launch_deadline_ms` | `300000`: time after its grant within which a granted attempt's agent must start; past it the server ends the attempt as not started and offers its task again (defaults only, current Settings apply) |

`max_attempts` and `max_uncharged_continuations` are a task's retry budget.
Each attempt has its own time window, the execution profile's `wall_time_ms`;
earlier attempts' time never counts against a retry. A launch pins them
with its policy, and the server reads them from the last attempt's pinned
policy when it decides on a replacement; it has no limits of its own. An
owner retry of a task a limit holds starts every budget again, the
never-granted count and the cost budget included. The first verified nonstart
can be retried as soon as its claim is released. The second consecutive
nonstart waits `initial_delay_ms`, then each further one doubles the delay
up to `max_delay_ms`. Both values are positive, and the maximum must be at
least the initial delay. The scheduler uses the current Selection defaults
and measures each delay from the latest claim release.

A save of the Selection defaults over HTTP keeps every key the request does
not state.

A profile overrides single keys under an optional `## Selection` heading, a
list of `- key: value` items:

```markdown
## Selection
- max_attempts: 3
- observation_max_age_ms: 30000
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
bytes for a name and a description. `DocumentLimits::default()` is 65,536; 1,048,576;
128; 128; 2,000.

### Startup text

`roles::assemble_startup` builds what the agent is told, in the order of
spec 0014 §6.1: platform text, the role's `## Role`, its `## Rules`, the
project's notes for the role, the repository's notes, the task, the reporting
contract. Absent or blank notes and absent rules are left out. The parts are
joined by one blank line (`utf8_double_newline_v1`). Text over the byte limit
the caller passes is refused; nothing is dropped to make it fit.
