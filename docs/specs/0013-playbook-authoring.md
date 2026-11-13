# 0013: Playbook authoring (one compiler from a Markdown document to a running playbook)

**Status:** written 2026-11-13 from the owner's direction of that day. Design
calls made without the owner are marked **(call)**.
**Repositories:** aq-playbooks (language, catalog, simulation library),
aq-server (authoring service, host commands), aq-dashboard.
**Replaces:** the generation, planner and playbook parts of spec 0011 (§5
"planner", §6, §7 "compiling the policy"). Spec 0011's route record,
placement, owner set/clear and task reads stay.

## 1. What the owner asked for

> I wanted the whole process to be generated from the current markdown that
> lives in settings. The goal is to use a generic playbook compiler that will
> compile any playbook, and the result should be a real playbook artifact
> that can do real routing in agent-queue. The same compiler should be able
> to make any playbook policy for the engine.

Spec 0011 did less than this. It had a model fill in a routing decision
table, and a playbook written by hand in the server apply it. That compiler
could compile nothing else, and the process was not in the document.

## 2. Goals

1. One authoring path: a Markdown document goes in, a playbook artifact
   comes out, and the engine runs it. Nothing in the path knows about
   routing.
2. The routing document in Settings > Intelligence, as it is written today,
   compiles through that path into the playbook that routes tasks.
3. The steps of a playbook come from its document. Changing the document
   changes the process, with no code change and no deploy.
4. The owner sees what a compiled playbook will do before trusting it, in
   terms of outcomes, not in terms of steps or identifiers.
5. A compile either produces a playbook that passed every check, or changes
   nothing and says which sentence is the problem.

## 3. Not goals

- A dashboard editor for arbitrary playbooks. The service is generic; the
  first and only document wired to it here is routing. **(call)**
- A playbook granting itself abilities. What a playbook may do is decided by
  the host catalog and the grant on its binding, never by its document.
- Keeping the routing decision table (`aq_routing::Policy`, `plan`,
  `compile`). It is removed.

## 4. The path

```
document (Markdown)                         Settings, or any stored source
   │  internal work, purpose "playbook compilation"
   ▼
proposal (structured: triggers, steps, conditions, limits, examples, questions)
   │  server fills exact contract identities; engine's deterministic compiler
   ▼
artifact  ── refused with diagnostics ──► back to the model, at most 3 times
   │  server runs the proposal's examples against the artifact
   ▼
checked artifact + what it does for each example
   │  install, activate on the binding, keep the previous one for rollback
   ▼
running playbook
```

### 4.1 Source

A source is a Markdown document plus a small record the server keeps: an id,
the scope, the binding it activates on, and which catalog commands and
events its playbook may use. The owner writes only the Markdown. The server
writes the engine's `---aq-playbook` header from the record, so the document
never contains identifiers (spec 0011 §2 stands).

The routing source is the existing `routing_markdown` field. Its record is
built in. An API to register other sources exists for owners and
supervisors, with no dashboard screen yet. **(call)**

### 4.2 The authoring request

One internal work request, purpose "playbook compilation". Its input:

- the Markdown;
- the playbook language, as a fixed reference text that ships with the
  engine: operations, expressions, limits, and the rules the compiler
  enforces;
- the commands and events this source may use, rendered from the pinned
  catalog by name with their inputs, outcomes and outputs, and no
  fingerprints;
- named facts the host offers this source as fixed definitions. For routing:
  the levels that have an assigned profile, their order, and the harnesses
  and providers the assigned profiles list.

Its answer is the engine's proposal, checked against a JSON Schema
published by the engine for the proposal type, plus:

- `questions`: sentences the model could not turn into steps without
  guessing. Any question refuses the compile.
- `examples`: at least 5 and at most 40 concrete cases (§4.4).

Nothing in the instructions is about routing. Routing knowledge reaches the
model only through the document, the catalog and the definitions.

### 4.3 Deterministic compile

The server, not the model:

1. writes the source header;
2. replaces each command, event and dependency name in the proposal with its
   exact contract identity from the pinned catalog, and builds the registry
   from what the proposal uses. A name outside the source's allowed set is an
   error;
3. captures the definitions by digest;
4. calls the engine's `compile`.

Structural errors go back to the model with the diagnostics, in the same
internal work task, at most 3 times **(call)**. Unresolved questions do not
go back; they go to the owner.

### 4.4 Examples: how the owner knows what it does

A model's own summary of what it wrote proves nothing. So the summary is
computed.

Each example is a starting situation in plain terms and in data: the
trigger's event or the entry's input, what each command the playbook calls
returns (including a judgment's answer), and the outcome the document says
should follow. The server runs every example against the compiled artifact
in the engine's simulator, with those returns as the fake host.

- An example whose simulated outcome differs from its stated outcome fails
  the compile. The diagnostics go back to the model (counted in the 3).
- An example that reaches a command the example gave no return for fails
  the compile.
- The owner sees the examples as a table of situation and result, taken
  from the simulation, for example "a bug fix that failed once → Complex,
  high effort, Claude or Codex". For routing the server adds fixed
  situations of its own, one per kind of work the document names and one
  per level, so the table does not depend only on cases the model chose.
  **(call)**

The simulator moves from the engine's CLI crate into a library crate the
server can call.

### 4.5 Taking effect

A compile that passes is installed and activated on the source's binding at
once (the owner's earlier decision: it takes effect immediately). The
previous artifact stays installed. The owner can roll back to it. Runs
already started finish on the artifact they started with.

A compile that fails changes nothing. The owner sees the questions, or the
last diagnostics in plain language with the sentence they point at, and the
previous playbook keeps running.

The view also shows a diff against the previous artifact in the engine's
terms: no change, wording only, behaviour changed, or abilities changed. A
compile that needs an ability the binding's grant does not give is refused,
not granted.

## 5. What the engine needs so a document can say real things

Found by reading the engine on 2026-11-13
(`.superpowers/sdd/2026-11-12-routing-policy/` mapping report). Each item is
generic, and each is needed by the routing document.

1. **Choosing by a value.** The language cannot look a value up by a key it
   learns at run time, so "this kind of work gets that level" would have to
   be unrolled into a chain of yes/no steps. Add a lookup expression: a
   literal table, a key expression, and a required default.
2. **Reading an answer.** After waiting for a judgment, a playbook cannot
   read the answer, because the completion event's output is a union and a
   wait does not narrow it (the D11 limitation). Make a wait on an event
   that declares an outcome field branch per outcome, with the output typed
   for that outcome.
3. **Asking for a structured answer.** `judgment.request` returns one label.
   Add an answer schema to the request, bounded in size and depth, so a
   classification with several fields is one judgment.
4. **A proposal schema.** Publish a JSON Schema for the proposal type,
   generated from the engine's types and tested against them.
5. **A language reference** written for a model: the text §4.2 sends.

Ordered comparison (`lt`) and membership exist and are enough for "after
one failure" and "at least Standard". No arithmetic is added. **(call)**

## 6. What the host catalog needs (catalog 2.2.0)

1. `context.read work` returns facts about the task: kind, title and
   description (bounded), origin, the level hint if any, how many attempts
   failed and were charged, and the current route if any.
2. `context.read execution_options`, keyed by task, returns what Settings
   offers: levels with an assigned profile and, per level, harnesses and
   providers.
3. `work.change` variant `route`: level, an optional minimum level, allowed
   harnesses and providers in order, and a reason. The host checks it
   against what Settings offers, applies the minimum, and writes the route
   record spec 0011 defines. Outcomes: routed, stale, refused.
4. Event `aq.work.route_needed`, as spec 0011 §6 describes it.
5. `judgment.request` with the answer schema of §5.3. A playbook's judgment
   runs as internal work; the purpose is "routing classification" when the
   run was started by `aq.work.route_needed` and "playbook call" otherwise.
   **(call)**

`routing_plan` from spec 0011 is not added.

## 7. Routing on this path

The routing document compiles to a playbook that starts on
`aq.work.route_needed`, reads the task, asks for a classification when the
document says one is needed, picks the level and who may take the work, and
writes the route. Placement honours the route exactly as built.

- The routing switch needs an active routing playbook, not an active policy
  row.
- The digest on a route is the artifact's digest.
- Failure handling (a task unrouted too long, a failed run, a route nobody
  can serve) is spec 0011 §4 W6, unchanged, and is written in the server,
  not in the document. **(call)** A broken playbook must not be the thing
  that reports itself broken.

## 8. What is removed or kept

| Piece | Fate |
| --- | --- |
| `aq_routing::{Class, Route, CompactRoute, RouteLimits, Placement}`; server `routing.rs` write/clear/history; placement; owner set/clear; task `routing` read; dashboard `TaskRoute` | Kept |
| `aq_routing::classify::{Options, Offer}` | Kept, as the definitions of §4.2 |
| `aq_routing::{policy, plan, compile, fixtures}`; `routing/compile.rs`; the Policy view and editor summary | Removed. The compile bookkeeping (row per compile, resume, supersede) is reused by the authoring service |
| Tables `routing_policies`, `route_needed_emissions` (migration 0120, applied on a live database) | Left in place. New tables in a new migration. A later migration may drop the unused one |
| Routing plan tasks 10–16 | Replaced by the plan for this spec |
| Internal work, the engine storage change (aq-playbooks spec 0003) | Unchanged, and required |

## 9. Acceptance

1. The routing document saved in Settings today compiles with no edit, on a
   subscription login, into an artifact that the engine's own verifier
   accepts.
2. With that artifact active on the test stack, tasks of each kind the
   document names are routed as the document says, shown by the route on
   each task, with no routing-specific code deciding the route.
3. Changing one sentence of the document that changes the process (for
   example "ask for a second opinion on high-risk work") changes the
   compiled steps and the example table, with no code change.
4. A second, unrelated document (a small review-reminder playbook) compiles
   and runs through the same service with the same instructions.
   This is the test that the compiler is generic.
5. A document with a sentence the catalog cannot serve is refused with that
   sentence, and the previous playbook keeps running.
6. An example that contradicts the compiled steps fails the compile.
7. 500 routing runs on the engine with spec 0003 keep step time flat.

## 10. Left to the builder

- The JSON form of examples and how a situation is rendered for the owner.
- The exact syntax of the lookup expression and of per-outcome waits.
- Table shapes for sources and compiles.
- The wording of the language reference.
