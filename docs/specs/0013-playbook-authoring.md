# 0013: Playbook authoring (an agent turns a Markdown document into a running playbook)

**Status:** written 2026-11-13 from the owner's direction of that day. Design
calls made without the owner are marked **(call)**.
2026-11-13: aligned with the rolling-records principle
([architecture 0006](../architecture/0006-records-are-rolling.md));
implementation pending. Authoring runs are a rolling set (§4.5).
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

1. One authoring path: an agent, with documentation and tools, turns a
   Markdown document into a playbook artifact that the engine runs. Nothing
   in the path knows about routing.
2. The routing document in Settings > Intelligence, as it is written today,
   compiles through that path into the playbook that routes tasks.
3. The steps of a playbook come from its document. Changing the document
   changes the process, with no code change and no deploy.
4. The owner sees what a compiled playbook will do before trusting it, in
   terms of outcomes, not in terms of steps or identifiers.
5. An authoring run either saves a playbook the engine accepts, or changes
   nothing and says why.
6. The person writing the Markdown needs to know nothing about how
   playbooks work.

## 3. Not goals

- A dashboard editor for arbitrary playbooks. The service is generic; the
  first and only document wired to it here is routing. **(call)**
- A playbook granting itself abilities. What a playbook may do is decided by
  the host catalog and the grant on its binding, never by its document.
- Keeping the routing decision table (`aq_routing::Policy`, `plan`,
  `compile`). It is removed.

## 4. The path

**Owner, 2026-11-13 (later the same day):** "I don't want a direct markdown
to artifact compiler either. I want an agent, using its own intelligence,
plus agent queue tools and documentation, to be able to assemble and save a
playbook artifact from markdown. This way it can be extremely flexible and
the person writing the markdown doesn't need to know specifics around how
playbooks work."

So the compiler is an agent, not a pipeline. There is no fixed
prompt-in, schema-checked-answer-out step, and the server does not drive a
repair loop. An agent is given the document, the documentation and tools,
works the way an engineer would (look things up, draft, check, try it,
fix it), and saves the playbook when it is satisfied.

```
document (Markdown)                         Settings, or any stored source
   │
   ▼
authoring agent ── reads: the authoring guide, the language reference
   │               tools: list and describe commands and events, read the
   │                      facts the host offers, check a draft, run it
   │                      against cases with a fake host, compare with the
   │                      previous playbook, ask the owner a question, save
   ▼
save ── the engine refuses a malformed playbook, as it does for any artifact
   │
   ▼
installed and active on the binding; the previous one kept for rollback
```

### 4.1 Source

A source is a Markdown document plus a small record the server keeps: an id,
the scope, the binding it activates on, and which catalog commands and
events its playbook may use. The owner writes only the Markdown. It never
contains identifiers (spec 0011 §2 stands); anything the engine's format
needs beyond the prose is the agent's job.

The routing source is the existing `routing_markdown` field. Its record is
built in. An API to register other sources exists for owners and
supervisors, with no dashboard screen yet. **(call)**

### 4.2 The authoring agent

Saving a source starts one authoring run. It is internal work with the
purpose "playbook compilation", in a new agent mode: the harness (Claude or
Codex, on a subscription login, chosen by the internal-work profile for
that purpose) runs as an agent with tools and a scratch directory, not as a
single answer. **(call: internal work, so it is never queued behind project
work and uses the same profiles, failover and failure path; the alternative
was a supervisor session.)**

The agent starts with:

- the Markdown, and the previous version and its playbook if there was one;
- the authoring guide (the engine's authoring skill, extended) and the
  language reference, as files;
- the source's record: what it is for, its binding, and what it may use.

Nothing it is given is about routing. Routing knowledge reaches it only
through the document and through what the tools return.

Its tools (the engine's CLI and the AQ CLI, already partly built):

| Tool | What it does |
| --- | --- |
| Discover | List and describe the commands and events this source may use, with inputs, outcomes and outputs |
| Facts | Read what the host offers this source as fixed facts. For routing: the levels with an assigned profile, their order, and the harnesses and providers those profiles list |
| Check | Validate a draft and get precise diagnostics. The tool fills in exact contract identities and digests, which an agent cannot compute by hand |
| Try | Run a draft against a case with a fake host: this event arrives, these commands return this, what happens and which commands were called with what |
| Compare | Diff a draft against the playbook currently active |
| Ask | Put a question to the owner when the document cannot be turned into steps without guessing. The run ends as "needs an answer" and nothing changes |
| Save | Submit the finished playbook with its cases |

How many drafts it writes, what it tries, and how it resolves a diagnostic
are up to the agent. The run is bounded by time and by cost, not by a
number of rounds **(call: 30 minutes, the internal-work deadline for this
purpose)**.

### 4.3 Save

Save is the one place the server checks anything, and it checks only what
it would check for a playbook from any author:

- the engine's own verification of the artifact (structure, contracts,
  limits), which install already runs;
- the playbook uses only what the source's record allows and what the
  binding's grant gives. A document cannot grant abilities by describing
  them;
- every case the agent submitted, re-run by the server against the saved
  artifact, ends the way the case says. **(call)** This is not a constraint
  on the agent's method. It keeps the table in §4.4 true.

A refused save returns the reason to the agent, which can fix it and save
again within its run.

### 4.4 Cases: how the owner knows what it does

An agent's own description of what it built proves nothing, and the owner
should not have to read steps. So the agent submits cases with the
playbook: a situation in plain terms and in data (what arrives, what each
command returns, including a judgment's answer) and the result the document
calls for. The server runs them against the saved artifact and shows the
owner a table of situation and result taken from that run, for example "a
bug fix that failed once → Complex, high effort, Claude or Codex".

At least 5 cases are required **(call)**. The authoring guide tells the
agent to cover each kind of situation the document distinguishes.

The simulator moves from the engine's CLI crate into a library the server
can call, and stays available to the agent through the CLI.

### 4.5 Taking effect

A saved playbook is installed and activated on the source's binding at once
(the owner's earlier decision: it takes effect immediately). The previous
artifact stays installed, and the owner can roll back to it. Runs already
started finish on the artifact they started with.

A run that ends without a save changes nothing. The owner sees why in plain
language: the agent's question, or what it could not make work and the
sentence involved. The previous playbook keeps running. The run itself is
visible like any internal work run, including what the agent did.

Authoring runs and their compile records are a rolling set: the server keeps
the most recent 20 per source **(proposed default)** and deletes the oldest
when a new run is written. The active and previous playbooks are installed
artifacts and are kept while installed.

The view also shows a diff against the previous artifact in the engine's
terms: no change, wording only, behaviour changed, or abilities changed.

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
4. **Tools an agent can drive.** The engine CLI's discover, check, try and
   compare commands accept a draft with names in place of exact identities,
   fill the identities in, and report diagnostics an agent can act on.
5. **An authoring guide and a language reference** written for an agent,
   with worked examples. No complete small example exists in the repository
   today.

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
4. A second, unrelated document (a small review-reminder playbook) is
   authored and runs through the same service with the same guide and
   tools. This is the test that the path is generic.
5. A document with a sentence the catalog cannot serve is refused with that
   sentence, and the previous playbook keeps running.
6. A save whose cases do not run as stated is refused.
7. 500 routing runs on the engine with spec 0003 keep step time flat.

## 10. Left to the builder

- The JSON form of examples and how a situation is rendered for the owner.
- The exact syntax of the lookup expression and of per-outcome waits.
- Table shapes for sources and compiles.
- The wording of the language reference.
