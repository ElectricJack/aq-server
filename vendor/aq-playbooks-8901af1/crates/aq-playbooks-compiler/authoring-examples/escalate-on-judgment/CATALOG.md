# Catalog

The commands and events this playbook may use, and what the host says about each. `aq-playbooks commands describe <name>` and `aq-playbooks events describe <name>` show the same contracts as JSON, with these notes. Where a meaning says "Not stated", the catalog does not say: do not rely on it, and ask the owner when the document depends on it.

## Commands

### `judgment.assess` 1.0.0

Asks a model several labelled questions about a subject at once. It answers later, on `aq.judgment.assessed`.

A change that answers later: its `accepted` edge binds `{handle}`, and the answer arrives as `aq.judgment.assessed`. Read it with a join. The host answers within 60000 ms. Needs: `judgment.assess`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `budget` | {deadline_ms, max_output_tokens} | `deadline_ms` (1000 to 600000), how long the host may take; `max_output_tokens`. 120000 ms and 2048 tokens are plenty for a classification. The host must refuse a budget above its caps with `budget_exceeded`. |
| `context` | {items} | `items`: at most 16 `{label, text}` the model sees, such as a task's title and description. |
| `fields` | array ≤ 16 of {choices, description, id} | Today the host has no adapter for this command, and what it does with no fields is not decided: include at least one field. One entry per labelled question: `{id, description, choices}`, at most 16, with up to 64 choices of at most 128 bytes. Include a choice such as `unknown` when the document says what to do if it cannot be told. |
| `flags` | array ≤ 32 of {description, id} | Yes/no questions: `{id, description}`, at most 32. The model says which hold. |
| `question` | string ≤ 4096 B | What to judge, in the document's own words, with its definitions. |
| `rationale_max_bytes` | integer 0..4096 | How long the model's explanation may be, 0 to 4096. |
| `subject` | {id, revision} | The subject being judged. Use the `subject` a read returned. |

| Outcome | Returns | When, and what next |
| --- | --- | --- |
| `accepted` | {handle} | The question is queued; the edge binds `{handle}`. Join the handle under a key, accepting only `assessed`. |
| `assessed` | {flags, labels, provenance, rationale, reference, subject} | `labels` has exactly one label per field, in field order (`labels.0` answers `fields[0]`), each one of that field's choices; `flags` lists the flags that hold, each once; `rationale`; `reference` identifies the judgment. Read these only on the join's `completed` edge. |
| `failed` | {reason, reference, subject} | No usable answer: `indeterminate`, `provider_error`, `provider_unavailable` or `deadline_exceeded`. Take the fallback the document gives. |
| `refused` | {reason, reference, subject} | The request was not run. The host must refuse it with `invalid_request` (over 65536 bytes, duplicate field ids, a field without choices) or `budget_exceeded`. It can come at once, on the invoke's own `refused` edge, or through the join. Take the fallback the document gives. |

The host must give every accepted judgment exactly one final answer, at the latest `failed` with `deadline_exceeded` when the host's own deadline passes. The join's deadline only decides when the run stops waiting.

Write a fallback that reads nothing of the answer on every way the judgment can fail: the join's `no_match`, `timeout` and `on_error`, and the invoke's own outcome edges besides `accepted`.

When a yes to a flag would make the work cost more, anchor it in the flag's description and the question, for example: "Most tasks do not need this. Answer yes only if you can name what is unusually hard."

Never invoke `operation.get` on its handle: read the answer through the join.

### `operation.get` 2.0.0

Reads the state of an operation a playbook started.

A read: safe to repeat. The host answers within 60000 ms. Needs: `operation.get`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `handle` | string ≤ 4096 B | The operation's handle, from the `accepted` edge. |

| Outcome | Returns | When, and what next |
| --- | --- | --- |
| `cancelled` | {reference, subject} | It was cancelled. |
| `completed` | {outcome, output} or {outcome, output} or {outcome, output} or {outcome, output} | It finished; `outcome` and `output` are its answer. |
| `failed` | {reference, subject} | Not stated. |
| `indeterminate` | {reference, subject} | The host cannot tell. |
| `refused` | {reference, subject} | Not stated. |
| `running` | {reference, subject} | It has not finished. |

It is the reconciliation command of the asynchronous commands, which the tool adds by itself. A playbook reads an answer through a join, not through this command; it does not carry `judgment.assess` answers.

### `work.read` 1.0.0

Reads the facts of a task as it is now.

A read: safe to repeat. The host answers within 10000 ms. Needs: `work.read`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `subject` | {id, revision} | The task's `{id, revision}`. Today the host reads the task as it is now, whatever revision is given. |

| Outcome | Returns | When, and what next |
| --- | --- | --- |
| `observed` | {facts, reference, subject} | `subject` carries the task's current revision: pass this subject on, not the trigger's, so that a later write to a changed task is refused as stale. `reference` is `<id>@<revision>` of the task as read (one value per task version). `facts` holds what a document can test (below). |
| `refused` | {reason, reference, subject} | The host refuses with `unknown_task` (there is no such task) or `not_on_router` (the task's project is not routed automatically). End the run: nothing is wrong with the playbook. |

Today the host answers only for a task in a project on the automatic router. If the document expects to read tasks of any project, ask the owner.

`facts`, all always present: `kind` (the kind whoever filed the task gave, such as `bugfix`, or `""`; a claim, not a classification); `title` (at most 512 bytes); `description` (at most 8192 bytes); `origin` (`owner`, `agent`, `supervisor`, `playbook` or `unknown`: who filed it); `level_hint` (the level whoever filed it asked for, or `""`); `charged_failures` (earlier attempts that failed for a real reason and were charged to the task: escalate on this, and only this); `ended_attempts` (earlier attempts that ended for any reason, outages included: never escalate on it); `route` (`none`, `router` or `owner`: who set the current route).

`facts` does not hold: who owns or is assigned the task, its priority or urgency, its status (open, finished, cancelled), when it was created or last changed, its project, labels or tags, or provider load. A document that tests any of these cannot be followed with this command: ask the owner. Never stand another field in for it.

### `work.route` 1.0.0

Writes who may take a task, at which level.

A change, made once per call: a repeat of the same call returns the first result. The host answers within 10000 ms. Needs: `work.route`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `classification` | {fields, flags, labels, rationale, reference} or null | The judgment the route rests on, `{reference, fields, labels, flags, rationale}` copied from an `assessed` answer, or `null`. |
| `harnesses` | array ≤ 8 of string ≤ 64 B | The harnesses that may take the task, at most 8; empty means any. |
| `hold` | boolean | `true` only for a sentence that tells some work to wait for a particular agent rather than go elsewhere. Otherwise `false`. |
| `level` | one of `complex-high`, `complex-low`, `complex-max`, `complex-medium`, `deep-high`, `deep-low`, `deep-max`, `deep-medium`, `simple-high`, `simple-low`, `simple-max`, `simple-medium`, `standard-high`, `standard-low`, `standard-max`, `standard-medium` | One of `definitions.levels` (a parent `simple`, `standard`, `complex` or `deep` and an effort `low`, `medium`, `high` or `max`, such as `standard-high`). If the document names a level the facts do not offer, ask; never round it. |
| `prefer` | array ≤ 8 of string ≤ 64 B | Harnesses to try first, in order; empty means no preference. |
| `providers` | array ≤ 8 of string ≤ 64 B | The providers that may serve it, at most 8; empty means any. |
| `reasons` | array ≤ 16 of {code, detail?} | Why, at most 16 `{code, detail?}`: every choice that set, raised, lowered or relaxed the level and every limit on who may take the task. `detail` is a fixed sentence of at most 256 bytes, best the document's own. A playbook cannot build text: write one reason per situation and select them with `lookup` or `filter`. |
| `subject` | {id, revision} | The task, at the revision the playbook decided on: the `subject` from `work.read`. |

| Outcome | Returns | When, and what next |
| --- | --- | --- |
| `refused` | {reason, reference, subject} | Nothing was written; end. `not_offered`: nothing at that level (or in a non-empty list) is offered, and the host raises the task itself; never try a route the document does not give. `owner_route`, `not_on_router`, `unknown_task`: end. |
| `routed` | {harnesses, prefer, providers, reference, subject} | The route is written, with the lists as the host kept them. End. |
| `stale` | {reference, subject} | The task changed after the read; nothing was written. End: the host asks again for the new version. |

Today the host intersects `harnesses`, `providers` and `prefer` with what Settings offers at the level and drops the rest, so write the lists the document implies without filtering them yourself. Never decide from `definitions.offers` that nothing can take a task: write the route and let the host refuse it.

If the owner chose the route (`facts.route` is `owner`), end without writing one.

These sentences need no step; today the host does them: holding a task until something can take it, raising a task nothing could ever take, when a route is needed, never choosing by priority or load, which model a harness runs, which failures count (`charged_failures`), and recording the decision on the task.

The worked example `escalate-on-judgment` reads, judges, looks the route up, writes it, and has a fallback route on every way the judgment fails.

### `work.unroutable` 1.0.0

Says that the document allows nobody to take a task.

A change, made once per call: a repeat of the same call returns the first result. The host answers within 10000 ms. Needs: `work.unroutable`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `reasons` | array ≤ 16 of {code, detail?} | Why, with the same codes as `work.route`; usually `policy` with the document's sentence. At least one. |
| `subject` | {id, revision} | The task: the `subject` from `work.read`. |

| Outcome | Returns | When, and what next |
| --- | --- | --- |
| `recorded` | {reference, subject} | The task is left unrouted and raised for the owner. End. |
| `refused` | {reason, reference, subject} | `no_reasons`, `owner_route`, `not_on_router` or `unknown_task`. End. |
| `stale` | {reference, subject} | The task changed after the read. End: the host asks again. |

Use it only where the document itself allows nobody for a task. For a task Settings cannot serve, write the route and let `work.route` refuse it.

## Events

### `aq.judgment.assessed` 1.0.0

The answer to `judgment.assess`.

**Sent:** Only as the answer to a `judgment.assess` a playbook made. Join the handle, which types the answer per outcome; do not wait on this event or start a playbook on it.

Correlation fields: `operation`. Kept 86400000 ms.

| Payload field | Type | Meaning |
| --- | --- | --- |
| `outcome` | one of `assessed`, `failed`, `refused` | `assessed`, `failed` or `refused`. |
| `output` | {flags, labels, provenance, rationale, reference, subject} or {reason, reference, subject} or {reason, reference, subject} | That outcome's output. |

### `aq.work.route_needed` 1.0.0

A task needs a route.

**Sent:** Today the host sends it on its own: for a ready task in a project on the automatic router that has no route; at once when the task changes to a new version, or is retried or reopened; again for the same version at most every two minutes while it stays unrouted, but not while a run started for that version is unfinished. After `not_offered` it is not sent again until the task, the active playbook or Settings changes; after `work.unroutable`, until the task or the active playbook changes.

Correlation fields: none. Kept 86400000 ms.

| Payload field | Type | Meaning |
| --- | --- | --- |
| `subject` | {id, revision} | The task's `{id, revision}`. Start the run with it as input. |

A trigger has no guard, so every emission starts a run.
