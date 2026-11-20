# Language reference

This is what `draft.json` may say, and what each diagnostic code means.
`GUIDE.md` says how to work, and `CATALOG.md` what each command and event
of this playbook does. Write a command or an event by its name only, as
every example here does; the tool fills in its identity.

## A draft

```json draft notify-when-ready
{"purpose": "Ask for a description when a task has none"}
```

| Field | Required | What it is |
| --- | --- | --- |
| `purpose` | no | One sentence. Defaults to `record.json`'s. |
| `entries` | yes | Named ways to start a run, each `{input, outcomes, start}`: the input's schema, the outcomes a run can end with (each with its output schema, 1 to 64), and the first step. |
| `steps` | yes | Named steps, each `{op, edges, on_error}`. |
| `triggers` | yes | Events that start a run: `{id, event, entry, input}`, where `input` builds the entry's input from `event` (`[]` for none). |
| `limits` | no | The playbook's own limits (below). All ten fields, or none. |
| `source_map` | no | `{"<step>": <line of source.md>}`. |
| `id`, `scope`, `required_capabilities` | no | Leave them out. The tool writes them, and refuses a value that differs. |

Names (steps, entries, bindings, keys) are 1 to 128 characters from
`A-Z a-z 0-9 _ - .`. A binding is never `input`, `event`, `definitions` or
`vars`, and no two steps bind the same name.

## Schemas

A schema is a closed type, written with `type`:

| Schema | Fields |
| --- | --- |
| `{"type": "null"}`, `{"type": "boolean"}` | |
| `{"type": "integer", "minimum": 0, "maximum": 100}` | `minimum` ≤ `maximum` |
| `{"type": "string", "max_length": 64, "values": ["a", "b"]}` | `max_length` 1 to 1,048,576 bytes; `values` (optional, at most 1024) makes it an enum |
| `{"type": "array", "items": {…}, "max_items": 8}` | `max_items` 1 to 100,000 |
| `{"type": "object", "properties": {…}, "required": ["k"]}` | at most 1024 properties; no other keys are allowed |
| `{"type": "nullable", "inner": {…}}` | the value or `null` |
| `{"type": "union", "variants": [{…}, {…}]}` | 2 to 16 alternatives |

Schemas nest at most 32 deep.

## Steps

Every step has `op`, `edges` (outcome name to next step) and `on_error`
(the next step when the operation itself fails). `on_error` is `null` only
on `decide` and `return`.

### invoke

Calls a command. `edges` has one entry for every outcome of the command
(`aq-playbooks commands describe <name>` lists them). The outcome's output
is bound to `bind` in the steps after it. `retry` is `null`, or
`{"max_attempts": 2..16, "backoff_ms": …, "outcomes": [<outcomes to retry>]}`.

```json step
{"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null,
        "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}},
 "edges": {"observed": "check", "refused": "unreadable"}, "on_error": "failed"}
```

An asynchronous command (its outcomes include `accepted`) binds
`{"handle": …}` on `accepted` and answers later; read the answer with a
`join`.

### decide

Branches on a boolean. `edges` has `true`, `false` and `unknown`; `unknown`
is taken when the predicate is not known, for example because a value it
reads is missing.

```json step
{"op": {"kind": "decide", "predicate": {"op": "eq", "left": {"op": "ref", "root": "task", "path": ["facts", "description"]}, "right": {"op": "literal", "value": ""}}},
 "edges": {"true": "remind", "false": "described", "unknown": "failed"}, "on_error": null}
```

### wait

With `"event": null` it is a timer: edge `timeout` after `deadline_ms`.

```json step
{"op": {"kind": "wait", "event": null, "correlation": {}, "minimum_revision": null, "deadline_ms": 600000, "bind": "waited"},
 "edges": {"timeout": "read"}, "on_error": "failed"}
```

With an event name it waits for that event: edges `received` (the payload
is bound) and `timeout`. `correlation` gives an expression for every one of
the event's correlation fields. `minimum_revision` needs an event with a
revision. `deadline_ms` is at most the event's retention and the playbook's
`max_duration_ms`. To read an asynchronous answer use a `join`, not a wait:
a wait binds the whole payload and does not type it per outcome.

### for_each

Runs `body` once per item of `collection`, one at a time (`concurrency`
must be 1), then goes to `next`. The body's last step has its edges back to
the `for_each` step. `item` is bound in the body only. `max_items` is at
most `limits.max_iterations`. `on_failure` is `stop` (leave by `on_error`)
or `continue` (go on with the next item).

```json step
{"op": {"kind": "for_each", "collection": {"op": "ref", "root": "input", "path": ["tasks"]}, "item": "t", "body": "read_one", "next": "done", "max_items": 10, "concurrency": 1, "on_failure": "continue"},
 "edges": {}, "on_error": "failed"}
```

### join

Waits for asynchronous operations. Each member is the binding of an invoke's
`accepted` edge, written `{"op": "ref", "root": "<bind>", "path": []}`.
`member_keys` names each member; the result, bound to `bind`, is
`{<key>: {participant, outcome, output}}`. `mode` is `all`, `any_accepted`
or `any_completion`. `accepted_outcomes` lists the outcomes that count; it
must be empty for `any_completion` and not empty for `any_accepted`.
`remaining` is `track` or `cancel`. Edges: `completed`, `no_match` (an
answer that is not accepted) and `timeout` (after `deadline_ms`).

```json step
{"op": {"kind": "join", "members": [{"op": "ref", "root": "j", "path": []}], "member_keys": ["c"], "mode": "all", "accepted_outcomes": ["assessed"], "deadline_ms": 180000, "remaining": "track", "bind": "answer"},
 "edges": {"completed": "route", "no_match": "assume", "timeout": "assume"}, "on_error": "assume"}
```

On `completed`, with `mode` `all` and `accepted_outcomes` listing only
outcomes that have a field, `answer.c.output.<field>` can be read. On
`no_match` and `timeout`, and in the other modes, a read of a member's
output may be refused; the message says why.

### return

Ends the run with one of the entry's outcomes. No edges, `on_error` null.

```json step
{"op": {"kind": "return", "outcome": "reminded", "output": {"op": "literal", "value": null}}, "edges": {}, "on_error": null}
```

### call

Calling another playbook is not offered to an authoring source (code
`call`).

## Expressions

Every expression is an object with `op`. None has a side effect. A value is
known, missing (a reference to something that is not there) or unknown.
Missing travels: an object, array, comparison, `map`, `filter` or `let`
body with a missing part is missing. Only `exists` stops it. A step whose
input is missing stops the run; a `decide` takes `unknown`; a `lookup` with a
missing key takes its default.

`literal`, any JSON value:

```json expr
{"op": "literal", "value": ["ensured", "changed"]}
```

`ref`, a root and a path of object keys or array positions (`"0"`). Roots
are `input`, `event` (in a trigger), `definitions` (the facts), a step's
binding, a `let` name or a collection item.

```json expr
{"op": "ref", "root": "definitions", "path": ["coordinators"]}
```

`object` and `array` build values:

```json expr
{"op": "object", "fields": {"subject": {"op": "ref", "root": "task", "path": ["subject"]}, "hold": {"op": "literal", "value": false}}}
```

```json expr
{"op": "array", "items": [{"op": "ref", "root": "input", "path": ["task"]}]}
```

`exists` is `false` for a missing value and `true` otherwise. It does not
make a later read of the value allowed (`GUIDE.md`, "`exists` does not make
a read allowed").

```json expr
{"op": "exists", "value": {"op": "ref", "root": "task", "path": ["facts"]}}
```

`not`, `and`, `or` take booleans. `and` and `or` evaluate every operand: an
unknown operand makes the result unknown, even `and(false, unknown)`.

```json expr
{"op": "and", "values": [{"op": "literal", "value": true}, {"op": "not", "value": {"op": "literal", "value": false}}]}
```

```json expr
{"op": "or", "values": [{"op": "literal", "value": false}, {"op": "not", "value": {"op": "exists", "value": {"op": "ref", "root": "input", "path": ["hint"]}}}]}
```

`eq` is JSON equality. `lt` compares two integers or two strings.
`contains` tests whether an array holds a value.

```json expr
{"op": "eq", "left": {"op": "ref", "root": "task", "path": ["facts", "origin"]}, "right": {"op": "literal", "value": "owner"}}
```

```json expr
{"op": "lt", "left": {"op": "literal", "value": 2}, "right": {"op": "ref", "root": "task", "path": ["facts", "charged_failures"]}}
```

```json expr
{"op": "contains", "collection": {"op": "ref", "root": "answer", "path": ["c", "output", "flags"]}, "value": {"op": "literal", "value": "small_and_tested"}}
```

`map` and `filter` go over an array of at most `max_items` items, binding
each to `item`. `filter` keeps the items whose `predicate` is true.

```json expr
{"op": "map", "collection": {"op": "filter", "collection": {"op": "ref", "root": "reasons"}, "item": "r", "predicate": {"op": "ref", "root": "r", "path": ["when"]}, "max_items": 8}, "item": "s", "value": {"op": "ref", "root": "s", "path": ["code"]}, "max_items": 8}
```

`difference` keeps the items of `left` whose `identity` (a path) is not
among `right`'s. Every item must have the identity, and no identity may
repeat.

```json expr
{"op": "difference", "left": {"op": "ref", "root": "input", "path": ["wanted"]}, "right": {"op": "ref", "root": "input", "path": ["done"]}, "identity": ["id"], "max_items": 16}
```

`lookup` chooses by a key from a literal table, with a required default.
At most 256 cases, each `when` at most 1024 bytes and distinct, and each a
value the key can take. Only the chosen case is evaluated. A missing key
takes the default, so a key must be a value that is always there.

```json expr
{"op": "lookup", "key": {"op": "ref", "root": "size"}, "cases": [{"when": "small", "then": {"op": "literal", "value": 600000}}, {"when": "large", "then": {"op": "literal", "value": 3600000}}], "default": {"op": "literal", "value": 1800000}}
```

`let` binds up to 16 names in order; each sees the ones before it, and the
body sees all. A name is never a root or another name in scope. Each value
is evaluated once. With `lookup` it gives `max(a, b)` without arithmetic:

```json expr
{"op": "let", "bindings": [{"name": "a", "value": {"op": "literal", "value": 5}}, {"name": "b", "value": {"op": "literal", "value": 7}}], "body": {"op": "lookup", "key": {"op": "lt", "left": {"op": "ref", "root": "a"}, "right": {"op": "ref", "root": "b"}}, "cases": [{"when": true, "then": {"op": "ref", "root": "b"}}], "default": {"op": "ref", "root": "a"}}}
```

Conditional lists: write each candidate with a `when` and keep the ones that
hold with `filter`, then `map` them to their final shape, as the `map`
example above does.

## Values that are not there

A reference to something that is not there evaluates to missing, and
missing travels (see "Expressions"). Ordinary typing does not see this: it
merges the outcomes a binding can hold into one object, in which a field
only one outcome has is merely optional. So `check` holds a draft to these
rules, keeping each binding's outcomes apart on every way to each step.

1. **Strict reads.** A reference is refused (`expression_type`) when it
   reads a field that some outcome lacks:
   - where several outcome edges of an invoke meet at one step;
   - among a join's accepted outcomes (every outcome, when
     `accepted_outcomes` is empty);
   - a join key that can be absent: on `timeout`, a member that has not
     answered, and in `any_accepted` or `any_completion` with several
     members, a member that was not the one selected.

   A field that is optional in its one schema stays readable, and so does a
   position of an array a schema describes (no schema states a length). A
   literal is read as the value it is: each row of a literal table keeps
   its own fields and a literal array its own length, so give every row of
   a table the same fields.
2. **Bound reads.** A reference to a binding is refused at a step that some
   way reaches without binding it: through the binding step's `on_error` (it
   failed, so nothing was bound) or a wait's `timeout`; from a branch that
   skips it; on the first pass of a loop that binds it later; after a
   `for_each` whose body binds it (the collection may be empty); and at an
   entry step reached again by a back edge. A `for_each` item is bound in the
   body only, not on `next` or `on_error`, nor in the loop's `collection`.
   Inside a body, a failing step is handled by the loop (`on_failure`
   `continue` goes on with the next item, `stop` leaves by the loop's
   `on_error`), and the check follows both.
3. **Names carry what they hold.** A `let` name, a name for a name, an
   object or array holding the value, a `lookup` that can return it, a
   `map`, `filter` or `difference` over it, and a collection item are all
   held to the same rule as the direct read, with the same reasons. A
   missing value bound to a `let` name does nothing until the name's value
   is needed, so a name used only inside `exists` is fine.

The one exemption is the operand of `exists`. A `lookup` key is not exempt,
even inside `exists`: a missing key takes the default without a trace. The
`identity` of a `difference` is not exempt either.

The diagnostic names the step and the place in it, the reference as
written, what a name in it stands for, every way that lacks the value (at
most twelve), and the remedy. Read a value only where it is always there:
after the outcome edge that returns it, or on the `completed` edge of a join
whose `accepted_outcomes` lists only outcomes that have it. A `decide` on an
outcome, or on `exists`, does not narrow anything: branch on the edge first,
and read the value in the step that edge leads to.

Asking a question and reading the answer, as `escalate-on-judgment` does:
`ask` invokes `judgment.assess` and binds the handle as `j` on `accepted`;
every other way goes to the fallback `assume`, which reads nothing of the
answer.

```json step
{"op": {"kind": "invoke", "command": "judgment.assess", "bind": "j", "retry": null,
        "input": {"op": "literal", "value": {"subject": {"id": "task-1", "revision": 1}, "question": "How risky is this change?", "context": {"items": []},
                  "fields": [{"id": "risk", "description": "How much damage the change could do.", "choices": ["low", "high", "unknown"]}],
                  "flags": [], "rationale_max_bytes": 512, "budget": {"deadline_ms": 120000, "max_output_tokens": 1024}}}},
 "edges": {"accepted": "await", "assessed": "assume", "failed": "assume", "refused": "assume"}, "on_error": "assume"}
```

`await` joins `j` under the key `c`, accepting only `assessed` (the `join`
example under "Steps"). On `completed`, `answer.c.output.labels.0` is the
`risk` label; on `no_match` (the answer was `failed` or `refused`) and
`timeout` it cannot be read. An answer that arrives before the run reaches
the join is kept for it, and one accepted at the deadline still counts.

## Limits

### What a playbook declares

`limits` is part of the playbook's meaning: set it from the document, or
leave it out for these defaults.

| Field | Default | Allowed |
| --- | --- | --- |
| `max_step_visits` | 10,000 | 1 to 1,000,000 |
| `max_children` | 100 | 0 to 10,000; also the most join members |
| `max_depth` | 8 | 1 to 32 |
| `max_concurrent_effects` | 16 | 1 to 1024 |
| `max_concurrent_children` | 16 | 1 to 1024 |
| `max_payload_bytes` | 1,048,576 | 1 to 16,777,216 |
| `max_history_bytes` | 16,777,216 | `max_payload_bytes` to 1,073,741,824 |
| `max_duration_ms` | 86,400,000 | 1 to 31,536,000,000; the whole run, from its start |
| `max_iterations` | 1000 | 1 to 1,000,000; every `for_each` `max_items` fits in it |
| `max_iteration_depth` | 4 | 1 to 16 |

`max_duration_ms` bounds the whole run from its start, not each wait. A run
that is still going when it passes stops, at whatever step it is waiting
in. So set it above the longest way through the run: every wait and join
deadline on that way, added up. A document that waits a day needs more than
the default. `check` refuses a way whose waits reach the limit, with
`deadline` (`GUIDE.md` shows it).

`check` cannot see a wait inside a `for_each` body, which may run many
times. A run that stops on the limit there fails its case under `try` with
"the run stopped at step … on the playbook's own duration limit".

### Evaluation

An expression is evaluated at most 32 deep and at most 10,000 elements: one
per expression evaluated, one per lookup case, and a `map` or `filter` body
once per item. A draft whose expression could pass either on its worst path
is refused when it is checked.

### Size of a draft

| Bound | At most |
| --- | --- |
| Steps | 256 |
| `draft.json` | 786,432 bytes |
| One expression | 200,000 nodes |
| All expressions | 1,000,000 nodes |
| Type nodes in scope over all steps | 250,000 |
| Shapes followed | 1,000,000 |

A reference counts the size of the type it reads: `task.facts.kind` counts
one node, `task` its whole type. What a `let` name stands for is counted
once, where it is written. So bind a table, or its result, to a `let` name
once and read the name; read the one field you need, not a whole table or
binding. A value built from the same name twice at every level doubles at
every level. A literal table has one shape for each row, also when the rows
have the same fields.

```json diagnostic limit=bytes
{"code": "draft_limit", "message": "…a draft may be at most 786432 bytes. Shorten it…"}
```

```json diagnostic limit=total
{"code": "expression_limit", "path": "draft.json", "message": "the draft's expressions expand to …; a draft may expand to at most 1000000…"}
```

```json diagnostic limit=scope
{"code": "expression_limit", "path": "draft.json", "message": "the draft's steps have … type nodes in scope in all…; a draft may have at most 250000…"}
```

```json diagnostic limit=shapes
{"code": "draft_limit", "path": "draft.json", "message": "the draft reads values that can have many different shapes too often to check: more than 1000000 shapes to follow in all…"}
```

## What the engine keeps

- A run that has finished leaves the engine's state at once. It can still
  be inspected from history, which keeps the most recent finished runs.
- Repeat protection is a rolling set. The engine remembers the most recent
  keys of events and requests (`replay_keep` of them), each within its
  window. A repeat it still remembers is a replay. A re-send after the key
  was forgotten is accepted as new, unless it carries a request time older
  than the window, which is refused as stale. Nothing the engine has
  forgotten or finished counts against a new run.
- A run that has returned but owns an operation that has not answered stays
  live, as `finishing`, until the operation settles. An asynchronous
  command that never answers is settled only by the host: the host must
  always send a final completion. A join's deadline only decides when the
  run stops waiting.

## Diagnostic codes

Each diagnostic has `code`, `path` (`<file>:<place>`), and `message`. Fix
the fault where `path` points. Codes marked "Host" come from the host's
files (`record.json`, `catalog.json`, `facts.json`), never from a draft. Do
not edit those files. Give up with
`aq-playbooks unable --file <file> <reason>`, which names that file and no
sentence of the document.

### workspace_file
A workspace file is missing, too large, not a regular file, a link, or not
UTF-8. Write `draft.json` as a plain file in the workspace root.

### draft_format
`draft.json` is not a draft: bad JSON, an unknown or missing field, or a
contract written as anything but a name. The message names the field and
the line and column. Fix it there.

### record_format
Host: `record.json` cannot be read.

### catalog_format
Host: `catalog.json` cannot be read.

### facts_format
Host: `facts.json` cannot be read.

### catalog
Host: `record.json` allows a command or event that `catalog.json` lacks, or
the catalog lacks a contract that a command needs.

### unknown_command
A command name the record does not allow. The message lists the allowed
names, closest first. Use one of them, or give up with `unable` if none can
do what the sentence asks.

### unknown_event
An event name the record does not allow, in a trigger or a wait. As for
`unknown_command`.

### call
A `call` step. An authoring source is offered no other playbooks: use the
commands it may use.

### capability
A command needs a capability the record does not give. Use another command,
or give up with `unable`.

### draft_identity
The draft sets `id`, `scope` or `required_capabilities` to something else
than the tool writes. Leave them out.

### source_map
A `source_map` entry names no step, or a line outside `source.md`.

### draft_limit
The draft has more than 256 steps, more than 786,432 bytes, or reads values
of many shapes too often. Shorten it, give literal rows fewer places to be
read, or ask the owner to split the document.

### expression_limit
An expression is too large, too deep, or could do too much work; or the
draft as a whole is. The message gives the number, the bound, and the name
or root that contributes most. Bind a value once with `let`, read only the
field you need, lower a `max_items`, split a table over 256 cases, or nest
a second `let` past 16 bindings.

### expression_type
An expression does not type, or reads a value that is not there on every
way to its step. The message names the reference, every way that lacks it,
and the remedy. Read the value only after the edge that returns it; see
`GUIDE.md`, "Rules for the draft".

### schema_compatibility
An expression's value does not fit where it is used: a command's input, an
entry's input or an outcome's output. The message shows both types. Build
exactly the fields the target lists, within its bounds.

### schema
A schema in the draft is malformed (see "Schemas").

### identifier
A step or entry name has characters outside `A-Z a-z 0-9 _ - .` or is
longer than 128. Rename it.

### binding
A step's `bind`, a `for_each` item or a `let` name is reused, reserved
(`input`, `event`, `definitions`, `vars`), or shadows another name. Choose
another name.

### step_reference
An edge, `on_error` or an entry's `start` names no step.

### unhandled_outcome
A step lacks an edge for one of its outcomes: every command outcome on an
invoke; `true`, `false`, `unknown` on a decide; `timeout` (and `received`)
on a wait; `completed`, `no_match`, `timeout` on a join.

### unhandled_error
A step other than `decide` or `return` has `on_error` null. Give it a step.

### unreachable_step
No entry reaches the step. Connect it or remove it.

### unbounded_cycle
A cycle of steps that does not go through a `for_each`. Loops are written
with `for_each` only.

### return_edges
A `return` has edges or `on_error`. Remove them.

### return_outcome
A `return` names an outcome its entry does not declare.

### outcomes
An entry declares no outcomes, or more than 64.

### retry
A `retry` is out of bounds: `max_attempts` 2 to 16, `backoff_ms` above 0
and within `max_duration_ms`, `outcomes` not empty and all outcomes of the
command.

### deadline
A wait's `deadline_ms` is 0 or longer than `limits.max_duration_ms`.

### wait_horizon
A wait on an event waits longer than the event is kept, or the event is not
offered in the playbook's scope.

### correlation
A wait on an event does not give an expression for exactly the event's
correlation fields.

### revision
`minimum_revision` on an event without a revision.

### timer_correlation
A timer (`"event": null`) has `correlation` or `minimum_revision`. Empty
them.

### iteration_limit
A `for_each` `max_items` is 0 or above `limits.max_iterations`, or is below
the collection's own `max_items`. (A collection that is not a bounded array
is `expression_type`.)

### unsupported_feature
A `for_each` with `concurrency` other than 1.

### join
A join with no members, more members than `limits.max_children`, a deadline
of 0 or above `max_duration_ms`, or `any_accepted` without
`accepted_outcomes`.

### join_member_keys
`member_keys` must be empty or name every member once, with valid names.

### join_member_type
A member is not a handle or child binding, or has a path inside the handle.
Write `{"op": "ref", "root": "<bind>", "path": []}`.

### join_qualifier
`any_completion` with `accepted_outcomes`. Use `any_accepted`.

### lookup_case
Two `when` are equal, or a `when` is not canonical JSON (a fraction, for
example). A `when` the key cannot take is `expression_type`.

### limits
A `limits` value is outside its allowed range (see "What a playbook
declares").

### model_limits
More than 128 entries or triggers, or no steps at all.

### trigger_identity
A trigger `id` is invalid or used twice.

### unknown_interface
A trigger names an entry the draft does not have.

### authority
A command or event is not offered in the playbook's scope. Host, unless the
draft uses a command the record does not intend.

### artifact_limit
The sealed playbook would exceed 16 MiB. Shorten the draft.

### contract_fingerprint
Host: a contract identity does not match the catalog.

### registry_identity
Host: a catalog command has an invalid name or version.

### contract_limits
Host: a catalog command lacks a bounded scope, payload or deadline.

### recovery
Host: a catalog mutation declares observation recovery.

### async_contract
Host: an asynchronous catalog command is inconsistent with its completion
event.

### public_projection
Host: a catalog command's public fields are not in its outcomes.

### sensitive_field
Host: a catalog contract's sensitive fields are not in its schemas.

### event_contract
Host: a catalog event has an invalid identity, producer, scope or bound.

### definition
Host: a captured fact has an invalid version or digest.

### source_digest
Host: the source digest is invalid.

### source_identity
Host: the source header does not match the record.

### source_interfaces
Host: the source header does not list the draft's entries.

### source_definitions
Host: the source header does not list the facts.

### source_header
Host: the document's header cannot be written.

### source_limit
Host: the document is larger than 4 MiB.

### unsupported_format
Host: an unknown format version.

### compiler_build
Host: the compiler identity is missing.

### canonical
Host: the playbook cannot be encoded canonically.

### artifact
Host: the playbook cannot be sealed.

### dependency
A callable dependency is invalid. An authoring draft has none (see `call`).

### dependency_selection
As `dependency`.

### recursive_call
As `dependency`.

### unknown_dependency
As `dependency`.
