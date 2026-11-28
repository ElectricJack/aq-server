---
name: aq-playbook-author
description: Turn an owner's plain-language document into an aq-playbooks playbook inside an authoring workspace - read it, write draft.json and cases, check, try and save; or ask the owner, or give up. Use for authoring this engine's workflows, not general AQ administration.
---

# Authoring guide

You turn the owner's document, `source.md`, into a playbook. The owner wrote
it in plain words and knows nothing about playbooks. The owner never reads
your draft. The owner sees one of three things: the cases you saved (each a
situation and what happens), your question, or why you gave up.

Nobody answers you while you work. When the document cannot be followed
without guessing, you ask the owner and stop. A guess that checks and saves
is the worst result: it runs, and nobody sees that it was a guess.

End with exactly one of `aq-playbooks save`, `aq-playbooks ask` or
`aq-playbooks unable`. Then stop: a second ending is refused.

## The workspace

| File | Who writes it | What it is |
| --- | --- | --- |
| `source.md` | the owner | The document. Lines are numbered from 1. |
| `record.json` | the host | What the playbook is for, and the commands, events and capabilities it may use. |
| `CATALOG.md` | the host | What each of those commands and events does: every input field, every outcome and what to do next, and whether the host sends each event on its own. Read it first. |
| `facts.json` | the host | Fixed facts, such as the people or choices the installation offers. Read them with `aq-playbooks facts`. |
| `catalog.json` | the host | The contracts as JSON. It is large: use `aq-playbooks commands describe` instead. |
| `draft.json` | you | The playbook. |
| `cases/*.json` | you | 5 to 40 cases. |
| `out/outcome.json` | the tools | Written by `save`, `ask` or `unable`. Never write it yourself. |
| `previous.artifact.json`, `previous/` | the host | When a playbook runs now: its artifact, and in `previous/` its document (`source.md`), draft and `cases/`. Read the previous document to see what the owner changed; use `aq-playbooks compare`, not the artifact. |
| `GUIDE.md`, `LANGUAGE.md` | the host | This guide; every operation, expression, rule and diagnostic code. |
| `examples/` | the host | Two complete worked workspaces. Read them before you write a draft. |

Never edit `source.md`, `record.json`, `facts.json`, `catalog.json` or
`CATALOG.md`. The host checks your outcome again against its own copies.

Run every tool from the workspace root, the directory that holds
`record.json`. Give only relative paths inside it, such as
`cases/task-gone.json`. A tool run anywhere else refuses:

```sh notify-when-ready in=cases
aq-playbooks check
```

```json output
{"error": "…: this is not an authoring workspace: it has no record.json. The authoring commands run only in the workspace directory itself (…), not in a subdirectory of it. Change to that directory and run the command again."}
```

```sh notify-when-ready
aq-playbooks try ../cases/task-gone.json
```

```json output
{"error": "../cases/task-gone.json: only a path inside the workspace, relative to it, is accepted (such as cases/bugfix.json); absolute paths and `..` are refused."}
```

Every tool prints JSON. A refusal exits with status 1 and says what to fix.
How to read this guide's snippets: the word after a fence's language names
the worked example a snippet runs in (`doc=` names the version of its
document it runs on, `in=` the directory it is run from, `limit=` the bound
a diagnostic is about). A `json output` block shows part of what the
command before it prints, with `…` for text left out.

## The workflow

1. Read `source.md`, `record.json` and `CATALOG.md`. Then read the facts:

   ```sh notify-when-ready
   aq-playbooks facts
   ```

   ```json output
   {"facts": {"owner": "user:owner-1"}, "facts_version": "settings-7", "use": "Read a fact in the draft as {\"op\": \"ref\", \"root\": \"definitions\", \"path\": [\"<name>\", …]}."}
   ```

2. The contracts, as JSON with the same notes as `CATALOG.md`:

   ```sh notify-when-ready
   aq-playbooks commands list
   aq-playbooks commands describe work.read
   aq-playbooks events list
   aq-playbooks events describe aq.work.changed
   ```

3. Go through the document sentence by sentence. Every sentence that
   changes what happens becomes a step, or a branch, or at least a case.
   For each one, find what it needs in `CATALOG.md`: the event that starts
   it, the data it tests, the command that does it. Decide now whether the
   document can be followed at all; see "When not to write a playbook". Ask
   or give up before you write anything.
   A document that says a playbook files, makes or adds a task starts on
   `aq.work.changed` with outcome `ensured`, which means the task exists
   now and is sent whether the task was new or already there; it never says
   which. Go on when what the sentence asks for also holds for a task that
   already existed (as in notify-when-ready); ask when it depends on the
   task being new. The event is sent only for a playbook's own call in this
   namespace, never when a person creates a task: when the document does
   not say who files the task, or means a person, ask, and do not start on
   this event.
4. Read the worked example closest to the document (`examples/`). Then write
   `draft.json`. `LANGUAGE.md` lists every operation and expression. Write
   commands and events by their names; the tool fills in their identities.
   Map each step to the line of its sentence in `source_map`.
5. Check the draft:

   ```sh notify-when-ready
   aq-playbooks check
   ```

   ```json output
   {"ok": true, "stage": "validation", "diagnostics": []}
   ```

   `stage` says how far the check got: `reading` (a workspace file cannot
   be read), `filling` (unknown names, identities, capabilities, calls or
   `source_map` lines; only those are listed while there are any), then
   `validation` (every other fault, all at once). Fix what it lists and run
   it again. Each fault has a `code`; `LANGUAGE.md` has an entry for each.
6. Write the cases (see "The case format") and try each one:

   ```sh notify-when-ready
   aq-playbooks try cases/task-gone.json
   ```

   ```json output
   {"name": "task-gone", "passed": true, "outcome": "unreadable", "problems": []}
   ```

7. When `previous.artifact.json` exists, compare. Make sure the change is
   the one the new document asks for:

   ```sh notify-when-ready
   aq-playbooks compare
   ```

   ```json output
   {"change": "executable", "abilities_changed": false, "diff": {"capabilities_added": [], "changed_commands": [], "changed_events": [], "changed_steps": ["remind", "wait"], "source_changed": true}}
   ```

   `change` is `identical`, `presentation` (only the document, `purpose` or
   `source_map` changed), `interface` (an entry's input or outcomes changed)
   or `executable` (what the playbook does changed). `abilities_changed` is
   true when a capability was added or a command or event changed.
8. Save. `save` checks again, runs every case, and writes the outcome only
   when all of them pass:

   ```sh notify-when-ready
   aq-playbooks save
   ```

   ```json output
   {"saved": true, "written": "out/outcome.json", "next": "The run is finished: end your work now. The host checks the outcome again."}
   ```

   A refused save writes nothing and lists `reasons`. Fix them and save
   again.

## When not to write a playbook

One rule decides between the two other endings. **Ask** when the owner could
settle it by answering or by rewording the document. **Give up** (`unable`)
when the document is clear but what it asks cannot be done with what the
host offers at all. When both apply, ask first.

Ask when:
- A time, an amount or a wording is not given, or is vague: "wait a while",
  "a few times", "send a reminder" with no text. Never pick a number or
  write the words yourself: a message's words come from the document.
- The document names something the facts do not offer, such as a level, an
  agent or a person not in `facts.json`. Never round it to the nearest one,
  never substitute another, never drop it.
- The document tests or uses something no command returns, such as who owns
  a task, how urgent it is, its status, or when it last changed. Check every
  outcome in `CATALOG.md` (its notes list what is not there).
- A sentence has two readings that would make the playbook act differently.
- The document needs more than a draft may hold (see "Limits"). A draft
  cannot call another playbook, so ask the owner to split the document.

Give up when:
- The document starts on something the host never sends as an event. Before
  you write a trigger, read the event's **Sent** line in `CATALOG.md`. A
  playbook whose trigger never fires is not a playbook.
- A sentence asks for something no command in `record.json` can do.
- One of the host's own files is at fault (a diagnostic says so): name that
  file, not a sentence.

A sentence that only describes what the host does by itself (`CATALOG.md`
says which) needs no step, and is no reason to give up.

### Never stand one thing in for another

This is the mistake no tool catches. The document says "If the task is
marked urgent, give it one minute instead", and nothing in `CATALOG.md`
says whether a task is urgent. A draft that tests the task's kind instead
checks clean, its cases pass because you wrote them, and it saves:

```json draft notify-when-ready
{"steps": {"check": {"op": {"kind": "decide", "predicate": {"op": "eq", "left": {"op": "ref", "root": "task", "path": ["facts", "kind"]}, "right": {"op": "literal", "value": "urgent"}}}, "edges": {"true": "remind", "false": "described", "unknown": "failed"}, "on_error": null}}}
```

The task's kind is what whoever filed it typed, not whether it is urgent.
Ask instead:

```sh notify-when-ready doc=stuck/urgent.md
aq-playbooks ask --line 6 "The document gives an urgent task one minute, but nothing this playbook can read says whether a task is urgent: it sees only a task's kind, title and description and who filed it. How should an urgent task be recognised, or should this sentence go?"
```

```json output
{"outcome": {"kind": "question", "sentences": [{"line": 6, "text": "If the task is marked urgent, give it one minute instead."}]}}
```

### How to ask

Write the question in the owner's words. Never name commands, steps, files
or fields. Name each sentence it is about by its line, or by its first and
last line when it spans several (`--lines A-B`, at most 8 lines). Put every
sentence that needs asking into one question, repeating `--line` and
`--lines`: 1 to 16 sentences, and a question of at most 4096 bytes.

```sh notify-when-ready doc=stuck/urgent.md
aq-playbooks ask --line 3 --line 6 "Should an urgent task also be asked about when a playbook only changes it? And how should an urgent task be recognised?"
```

```json output
{"outcome": {"kind": "question", "sentences": [{"line": 3}, {"line": 6}]}}
```

A sentence over two lines, named whole:

```sh escalate-on-judgment doc=stuck/deep-level.md
aq-playbooks ask --lines 17-18 "The document gives high risk work Deep, high effort, but Settings offers no Deep level: it offers Simple, Standard and Complex, all at high effort. Which level should high risk work get?"
```

```json output
{"outcome": {"kind": "question", "sentences": [{"line": 17, "end_line": 18, "text": "- High risk work is Deep, high effort, because mistakes there are costly,\n  and only Claude or Codex may take it."}]}}
```

A line that does not hold its whole sentence is refused:

```sh notify-when-ready
aq-playbooks ask --line 10 "Which people?"
```

```json output
{"error": "line 10: the sentence goes on in line 11. Name the whole sentence, to its last line, such as --lines 10-12."}
```

### How to give up

Name the one sentence and say why, plainly, in at most 4096 bytes:

```sh notify-when-ready doc=stuck/phone-call.md
aq-playbooks unable --line 14 "No command this playbook may use can make a phone call: it can read a task and send a message, and nothing else."
```

```json output
{"outcome": {"kind": "unable", "line": 14, "sentence": "If it still has no description after a day, phone the person on duty."}}
```

For a fault in one of the host's files, name the file instead:

```sh notify-when-ready
aq-playbooks unable --file catalog.json "record.json allows the command message.send, but catalog.json has no contract for it."
```

```json output
{"outcome": {"kind": "unable", "file": "catalog.json"}}
```

The run then ends; a second ending is refused:

```sh notify-when-ready doc=stuck/how-long.md
aq-playbooks ask --line 3 "How long should a new or changed task wait before you are sent a message asking for its description?"
aq-playbooks unable --line 3 "The document gives no time."
```

```json output
{"error": "out/outcome.json already holds this run's ending (question); a run ends once. The run is finished: end your work now. The host checks the outcome again."}
```

## Sentences about many runs

A run starts from one event and knows nothing of other runs. So a sentence
such as "never send more than one reminder for the same version of a task"
cannot be kept by any step's logic, and no case can show it, since a case is
one run. It can be kept only by a command that the host itself makes do a
thing once: `CATALOG.md` says which do, and by what. For example, the host
sends one `message.send` per `identity`, so an identity that names the task
version (`task.reference` from `work.read`) keeps that sentence. A step
maps to one line in `source_map`: map that step to this sentence when it
has no sentence of its own. If no command the record allows promises it,
the document's promise cannot be kept: ask the owner.

## Rules for the draft

Each rule shows a right way, a wrong way, and what `check` says about the
wrong way. The snippets change steps of the worked examples; read the full
example drafts to see each step in place.

### Write names, not identities

Write a command or event by its name. The tool fills in its identity.

```json draft notify-when-ready
{"steps": {"read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}}, "edges": {"observed": "check", "refused": "unreadable"}, "on_error": "failed"}}}
```

Wrong: `"command": "work.reed"`.

```json draft notify-when-ready
{"steps": {"read": {"op": {"kind": "invoke", "command": "work.reed", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}}, "edges": {"observed": "check", "refused": "unreadable"}, "on_error": "failed"}}}
```

```json diagnostic
{"code": "unknown_command", "path": "draft.json:steps.read.op.command", "message": "steps.read.op.command: \"work.reed\" is not a command this source may use. Use one of the commands this source may use (closest first): work.read, message.send."}
```

An identity object such as `{"name": "work.read", "fingerprint": …}` is
refused the same way, with code `draft_format`.

### Give every outcome an edge

An invoke needs an edge for every outcome of its command, and every step
except `decide` and `return` needs `on_error`. Wrong: no edge for
`refused`.

```json draft notify-when-ready
{"steps": {"read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}}, "edges": {"observed": "check"}, "on_error": "failed"}}}
```

```json diagnostic
{"code": "unhandled_outcome", "path": "draft.json:steps.read", "message": "outcome refused has no edge…"}
```

### Read a value only after the edge that returns it

Outcomes of a command return different fields. Here `observed` returns
`facts` and `refused` does not. Read `task.facts` only in a step that the
`observed` edge leads to, as the example does (`"refused": "unreadable"`).
Wrong: both edges lead to `check`, which reads `task.facts.description`.

```json draft notify-when-ready
{"steps": {"read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}}, "edges": {"observed": "check", "refused": "check"}, "on_error": "failed"}}}
```

```json diagnostic
{"code": "expression_type", "path": "draft.json:steps.check", "message": "steps.check.predicate.eq.left: `task.facts.description` is not there on every way to this step: outcome `refused` of step `read` has no `facts` in `task` (it has reason, reference, subject). Read it only where it is always present…"}
```

### `exists` does not make a read allowed

`exists` only tells you whether a value is there. Testing it does not make a
later read of the value allowed, because the check does not narrow by a
test. Branch on the outcome edge first. Wrong: a `decide` on `exists` in
front of `check`.

```json draft notify-when-ready
{"steps": {"read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}}, "edges": {"observed": "has", "refused": "has"}, "on_error": "failed"}, "has": {"op": {"kind": "decide", "predicate": {"op": "exists", "value": {"op": "ref", "root": "task", "path": ["facts"]}}}, "edges": {"true": "check", "false": "unreadable", "unknown": "failed"}, "on_error": null}}}
```

```json diagnostic
{"code": "expression_type", "path": "draft.json:steps.check", "message": "`task.facts.description` is not there on every way to this step: outcome `refused` of step `read` has no `facts` in `task`…"}
```

### Never let a lookup hide a missing value

A `lookup` whose key is missing takes its default, without a trace. So a
lookup key is held to the same rule as any read, even inside `exists`.
Wrong:

```json draft notify-when-ready
{"steps": {"read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}}, "edges": {"observed": "check", "refused": "check"}, "on_error": "failed"}, "check": {"op": {"kind": "decide", "predicate": {"op": "lookup", "key": {"op": "ref", "root": "task", "path": ["facts", "description"]}, "cases": [{"when": "", "then": {"op": "literal", "value": true}}], "default": {"op": "literal", "value": false}}}, "edges": {"true": "remind", "false": "described", "unknown": "failed"}, "on_error": null}}}
```

```json diagnostic
{"code": "expression_type", "path": "draft.json:steps.check", "message": "steps.check.predicate.lookup.key: `task.facts.description` is not there on every way to this step: …A lookup whose key is missing takes its default without a trace, so a lookup key is held to this too…"}
```

### A value bound inside a loop body is not there after the loop

A `for_each` may run its body zero times. So a binding made only inside the
body cannot be read after the loop, nor in the loop's own `collection`.
Wrong: `read` is the loop's body, and `remind` reads `task` after it.

```json draft notify-when-ready
{"steps": {"wait": {"op": {"kind": "wait", "event": null, "correlation": {}, "minimum_revision": null, "deadline_ms": 600000, "bind": "waited"}, "edges": {"timeout": "each"}, "on_error": "failed"}, "each": {"op": {"kind": "for_each", "collection": {"op": "array", "items": [{"op": "ref", "root": "input", "path": ["task"]}]}, "item": "t", "body": "read", "next": "check", "max_items": 1, "concurrency": 1, "on_failure": "stop"}, "edges": {}, "on_error": "failed"}, "read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null, "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "t", "path": []}}}}, "edges": {"observed": "each", "refused": "each"}, "on_error": "failed"}}}
```

```json diagnostic
{"code": "expression_type", "path": "draft.json:steps.remind", "message": "steps.remind.input.identity: `task.reference` is not there on every way to this step: on the way from the start of entry `main` that does not pass step `read`, `task` is not bound…"}
```

The same holds after a branch that skips the binding step, on a step's
`on_error` way (nothing was bound), and on a wait's `timeout` way.

### Read an asynchronous answer through a join, on `completed`

An asynchronous command (one whose outcomes include `accepted`) answers
later. Its `accepted` edge binds a handle. Join the handle by its binding
alone, under a key, accept only the outcomes that carry what you read, and
read the answer only in a step the join's `completed` edge leads to. Right
(in escalate-on-judgment, `peek` reads the answer on `completed`):

```json draft escalate-on-judgment
{"steps": {"await": {"op": {"kind": "join", "members": [{"op": "ref", "root": "j", "path": []}], "member_keys": ["c"], "mode": "all", "accepted_outcomes": ["assessed"], "deadline_ms": 180000, "remaining": "track", "bind": "answer"}, "edges": {"completed": "peek", "no_match": "assume", "timeout": "assume"}, "on_error": "assume"}, "peek": {"op": {"kind": "decide", "predicate": {"op": "eq", "left": {"op": "ref", "root": "answer", "path": ["c", "output", "labels", "0"]}, "right": {"op": "literal", "value": "unknown"}}}, "edges": {"true": "assume", "false": "route", "unknown": "assume"}, "on_error": null}}}
```

Wrong: the same `peek` on `no_match`, where the answer failed or was
refused.

```json draft escalate-on-judgment
{"steps": {"await": {"op": {"kind": "join", "members": [{"op": "ref", "root": "j", "path": []}], "member_keys": ["c"], "mode": "all", "accepted_outcomes": ["assessed"], "deadline_ms": 180000, "remaining": "track", "bind": "answer"}, "edges": {"completed": "route", "no_match": "peek", "timeout": "assume"}, "on_error": "assume"}, "peek": {"op": {"kind": "decide", "predicate": {"op": "eq", "left": {"op": "ref", "root": "answer", "path": ["c", "output", "labels", "0"]}, "right": {"op": "literal", "value": "unknown"}}}, "edges": {"true": "assume", "false": "route", "unknown": "assume"}, "on_error": null}}}
```

```json diagnostic
{"code": "expression_type", "path": "draft.json:steps.peek", "message": "steps.peek.predicate.eq.left: `answer.c.output.labels.0` cannot be read: `answer.c.output` may be null at this step, so `labels` cannot be read. A join's answer (`outcome`, `output`) is present only on the join's `completed` edge…"}
```

Leaving `accepted_outcomes` empty accepts every outcome, so the read is
refused on `completed` too. A member with a path inside the handle, such as
`["handle"]`, is refused with `join_member_type`.

Every way the answer can fail needs a step that reads nothing of it: the
join's `no_match`, `timeout` and `on_error`, and the invoke's own outcome
edges besides `accepted` (a command can refuse at once, without a handle).
Give each of them a case (see "The case format").

### Set the playbook's own limits from the document

`limits` (all ten fields, see `LANGUAGE.md`) is part of what the playbook
means. `max_duration_ms` covers the whole run from its start, not each
wait: a run still going when it passes stops where it is. Leave `limits` out
for the defaults (a day), or set each limit from what the document says:
a wait of a day needs a duration limit above a day. Never set a limit at
its maximum for no reason. Wrong: a wait of a day under the default limit.

```json draft notify-when-ready
{"steps": {"wait": {"op": {"kind": "wait", "event": null, "correlation": {}, "minimum_revision": null, "deadline_ms": 86400000, "bind": "waited"}, "edges": {"timeout": "read"}, "on_error": "failed"}}}
```

```json diagnostic
{"code": "deadline", "path": "draft.json:limits", "message": "limits.max_duration_ms: a run from entry `main` may wait 86400000 ms on one way, at `wait` (86400000 ms), which reaches the playbook's duration limit of 86400000 ms (the default). The limit covers the whole run from its start…"}
```

Right: the same wait, with the duration limit set above it and the other
limits at their defaults.

```json draft notify-when-ready
{"steps": {"wait": {"op": {"kind": "wait", "event": null, "correlation": {}, "minimum_revision": null, "deadline_ms": 86400000, "bind": "waited"}, "edges": {"timeout": "read"}, "on_error": "failed"}}, "limits": {"max_step_visits": 10000, "max_children": 100, "max_depth": 8, "max_concurrent_effects": 16, "max_concurrent_children": 16, "max_payload_bytes": 1048576, "max_history_bytes": 16777216, "max_duration_ms": 90000000, "max_iterations": 1000, "max_iteration_depth": 4}}
```

`check` adds up the waits on every way through the run, but not inside a
`for_each` body, which may run many times. A run that stops on the limit
there fails its case under `try` with "the run stopped at step … on the
playbook's own duration limit".

### Map steps to sentences

`source_map` maps a step to the line of `source.md` it comes from. The host
shows the owner that sentence instead of the step. A line outside the
document is refused:

```json draft notify-when-ready
{"source_map": {"ready": 3, "wait": 3, "read": 6, "check": 6, "unreadable": 7, "remind": 13}}
```

```json diagnostic
{"code": "source_map", "path": "draft.json:source_map.remind", "message": "source_map.remind: line 13 is not a line of source.md, which has lines 1 to 12."}
```

## The case format

A case is one situation in plain words and in data: how the run starts, what
each command the run calls returns, and what must happen. The owner sees
each case's `situation` and the outcome it ends with, so write `situation`
in the owner's words, without step or command names.

```json case notify-when-ready
{
  "name": "task-gone-meanwhile",
  "situation": "The task is deleted before the ten minutes are up",
  "start": {"kind": "event", "event": "aq.work.changed",
            "payload": {"outcome": "ensured", "reference": "task-7@1", "subject": {"id": "task-7", "revision": 1}},
            "correlation": {"operation": "work-op-7"}},
  "returns": {
    "work.read": [{"outcome": "refused",
                   "output": {"reference": "task-7@1", "subject": {"id": "task-7", "revision": 1}, "reason": "unknown_task"}}]
  },
  "expect": {"outcome": "unreadable",
             "calls": [{"command": "work.read", "input": {"subject": {"id": "task-7", "revision": 1}}}]}
}
```

The fields:
- `name`: short and unique, at most 128 bytes. Name the file after it:
  `cases/<name>.json`.
- `situation`: at most 1024 bytes.
- `start`: how the run starts. Either an event a trigger names, with its
  whole `payload` and a value for each of the event's correlation fields
  (`events describe` lists them as `correlation_fields`; `{}` or leave it
  out when there are none):

  ```json start
  {"kind": "event", "event": "aq.work.changed", "payload": {"outcome": "changed", "reference": "task-40@4", "subject": {"id": "task-40", "revision": 4}}, "correlation": {"operation": "work-op-2"}}
  ```

  or an entry with its input:

  ```json start
  {"kind": "entry", "entry": "main", "input": {"task": {"id": "task-40", "revision": 2}, "change": "ensured"}}
  ```

  Start from the event when the playbook has a trigger: that also tests how
  the trigger builds the input.
- `returns`: for each command, one return per call, in call order. A return
  is `{"outcome", "output"}`, where `output` fits that outcome (see
  `commands describe`). For an asynchronous command there are three forms:
  - its answer, which the tool delivers later through the completion event,
    after answering `accepted` with a handle by itself, so the run goes
    through the join:

    ```json returns
    {"judgment.assess": [{"outcome": "assessed", "output": {"reference": "judgment:op-1", "subject": {"id": "task-12", "revision": 4}, "labels": ["low"], "flags": [], "rationale": "Only documentation changes.", "provenance": {"context_digest": "context-1", "credential_profile": "profile/judge", "model": "model-a", "output_digest": null, "prompt_digest": "prompt-1", "provider": "provider-a", "usage": {"cost_microunits": 900, "input_tokens": 400, "output_tokens": 60}}}}]}
    ```

  - an answer at once, `"at_once": true`, with one of its own outcomes
    other than `accepted`: the invoke's own edge for that outcome is taken,
    as when the host refuses a request at admission:

    ```json returns
    {"judgment.assess": [{"outcome": "refused", "output": {"reference": "judgment:op-1", "subject": {"id": "task-12", "revision": 4}, "reason": "budget_exceeded"}, "at_once": true}]}
    ```

  - no answer, `{"pending": true}`: the clock moves on to each deadline, so
    the playbook's timeout way runs.

    ```json returns
    {"judgment.assess": [{"pending": true}]}
    ```

  At most 64 returns per command. A return the run never uses fails the
  case.
- `expect.outcome`: the entry outcome the run must end with.
- `expect.calls`: every call the run makes, in order, each with its whole
  `input`, compared exactly. `[]` when the run calls nothing.

What `try` reports when a case and the run differ:

```json case notify-when-ready
{"name": "listed-wrongly", "situation": "A task a playbook filed still has no description after ten minutes",
 "start": {"kind": "event", "event": "aq.work.changed", "payload": {"outcome": "ensured", "reference": "task-40@2", "subject": {"id": "task-40", "revision": 2}}, "correlation": {"operation": "work-op-1"}},
 "returns": {"work.read": [{"outcome": "observed", "output": {"reference": "task-40@2", "subject": {"id": "task-40", "revision": 2}, "facts": {"kind": "", "title": "Add a dark theme", "description": "", "origin": "owner", "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "none"}}}],
             "message.send": [{"outcome": "queued", "output": {"reference": "message-1", "subject": {"id": "task-40", "revision": 2}}}]},
 "expect": {"outcome": "reminded",
            "calls": [{"command": "work.read", "input": {"subject": {"id": "task-40", "revision": 2}}},
                      {"command": "message.send", "input": {"identity": "task-40", "recipient": "user:owner-1", "visibility": "project", "body": "This task has had no description for ten minutes. Please add one, so that it can be worked on."}}]}}
```

```json report
{"passed": false, "problems": ["expect.calls[1].input: the call to message.send at step remind differs from the case: input.identity: the case expects \"task-40\", the call has \"task-40@2\"."]}
```

Other problems name the place the same way: a call the case does not list,
a listed call the run does not make, a call the case gives no return for
(the run then takes its error way), a return never used, or the wrong
outcome. A malformed case file is refused with its line and column.

How many cases, and which:
- At least 5 and at most 40; `save` refuses any other number.
- One case for each situation the document tells apart, and for each way a
  command can answer that the playbook handles: every outcome edge, every
  fallback, a value just below and just at each threshold the document
  gives.
- Cases that only differ in data the playbook does not look at add nothing.

What a case cannot show:
- What happens over many runs (see "Sentences about many runs").
- A completion event is delivered only to the operation that waits for it.
  It does not start a run through a trigger that names it, so a playbook
  that starts on its own completion event shows one run in a case.
- A run that has returned, and waits only for an operation that never
  answers, ends for the case with the outcome it returned.
- Together, a save's cases may take at most 40,000 engine transitions, and
  one case at most 10,000; a case file is at most 65,536 bytes.

## Limits

A draft is refused, before it is checked, when it is larger than these.
Shorten it as the message says. If the document really needs more, ask the
owner to split the document: a draft cannot call another playbook.

| Bound | At most | Code |
| --- | --- | --- |
| Steps | 256 | `draft_limit` |
| `draft.json` | 786,432 bytes | `draft_limit` |
| One expression, every reference counted at the size of the type it reads | 200,000 nodes | `expression_limit` |
| All expressions | 1,000,000 nodes | `expression_limit` |
| Type nodes in scope over all steps | 250,000 | `expression_limit` |
| Shapes followed (one per literal row, even rows with the same fields) | 1,000,000 | `draft_limit` |
| `lookup` | 256 cases, each `when` 1024 bytes | `expression_limit` |
| `let` | 16 bindings | `expression_limit` |
| `out/outcome.json` (the draft and every case) | 4,194,304 bytes | refused by `save` |

```json diagnostic limit=steps
{"code": "draft_limit", "path": "draft.json:steps", "message": "steps: the draft has 257 steps; a draft may have at most 256. Shorten it: one step per decision, with `lookup` and `let` for tables instead of chains of steps. A draft cannot call other playbooks, so if the document needs more than that, ask the owner to split the document."}
```

Build a value once. Bind a table, or its result, to a `let` name once and
read the name. Read the one field you need, not a whole table or binding.
A value built from the same name twice at every level doubles at every
level, and is refused:

```json diagnostic limit=expression
{"code": "expression_limit", "message": "…an expression may expand to at most 200000. Most of it is `b15` (counted at the size of the type it reads)…Build each value from a name once, not twice at every level, and read the one field you need rather than a whole binding or table."}
```

`LANGUAGE.md` gives the message for each of the others.

## The worked examples

`examples/` holds two complete workspaces, each with its `CATALOG.md`. Read
their `source.md`, `draft.json` and `cases/` before you write yours, and
imitate them.
- `notify-when-ready`: a timer, a read, a branch and a message, with no
  judgment. Its `stuck/` documents show a vague time (`ask`), a test of
  something no command returns (`ask`), and a request no command can serve
  (`unable`).
- `escalate-on-judgment`: a judgment, a join, a `lookup` table bound once
  with `let`, a fallback on every way the judgment fails (an answer at
  once included), and 18 cases. Its `stuck/` document shows a choice the
  facts do not offer (`ask --lines`).

Each `stuck/*.json` holds `why`, the command to `run`, and what it `writes`.
