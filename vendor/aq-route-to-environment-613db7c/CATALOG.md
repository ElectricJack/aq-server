# Catalog

The commands and events this playbook may use, and what the host says about each. `aq-playbooks commands describe <name>` and `aq-playbooks events describe <name>` show the same contracts as JSON, with these notes. Where a meaning says "Not stated", the catalog does not say: do not rely on it, and ask the owner when the document depends on it.

## Commands

### `work.read` 2.5.0

Reads the facts of a task as it is now, with those of its latest result (whether it changed anything, its branch and pull request, and how many times delivery has sent it back) and its sandbox environment facts: what it needs and which environments its project allows.

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

The facts of the task's latest result, always present, and empty when the task has no result yet: `outcome` (the outcome the agent reported for the result, at most 64 bytes, or `""`; the catalog does not list its values, so compare it only with a value the document names, and ask the owner what a value means when the document does not say); `changed` (boolean: the result differs from `base`; `false` when there is no result); `branch` (the result's branch in the central git, `aq/<task-id>`, or `""`); `head` (the commit the branch points at, or `""`); `base` (the commit of main the result was made on, or `""`); `main_head` (the commit main points at now, or `""`; it differs from `base` when main moved after the result was made); `returns` (integer 0..1000: how many times delivery has sent the task back to its agent with `work.return`); `pull_request` (`null` until `pull_request.open` has opened one, otherwise `{number, url, state, mergeable, checks}`: `state` is `open`, `closed` or `merged`; `mergeable` is `mergeable`, `conflicting` or `unknown`, and `unknown` means the forge has not decided yet; `checks` is `{state, failed}` where `state` is `none` (the pull request has no checks), `pending`, `passed` or `failed`, and `failed` lists at most 64 names of the checks that failed, each at most 256 bytes).

A fact is what the host knew when it answered. Do not stand it in for a later state: `pull_request.checks` read here is not the outcome of `pull_request.await_checks`, and `main_head` can move after the read. To act on a pull request, use the delivery commands, not these facts.

To send a task back only while it has been sent back fewer than N times, test `returns` with `lt` against the document's N. `returns` counts only `work.return`; `charged_failures` and `ended_attempts` count attempts, not deliveries.

`facts` does not hold: who owns or is assigned the task, its priority or urgency, its status (open, finished, cancelled), when it was created or last changed, its project, labels or tags, or provider load. A document that tests any of these cannot be followed with this command: ask the owner. Never stand another field in for it.

The environment facts, always present: `needs` (at most 64 capability names the task asks for, from its `aq:needs` metadata; empty when it asks for none); `default_environment` (the id of the project's default environment, or `""` when it has none); `environments` (the project's allowed environments, at most 64 `{id, provides}`, where `provides` lists at most 64 capability names the environment provides). To choose an environment, pick one whose `provides` holds every name in `needs`; the document says how to choose among them.

### `work.route` 2.5.0

Writes who may take a task, at which level.

A change, made once per call: a repeat of the same call returns the first result. The host answers within 10000 ms. Needs: `work.route`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `classification` | {fields, flags, labels, rationale, reference} or null | The judgment the route rests on, `{reference, fields, labels, flags, rationale}` copied from an `assessed` answer, or `null`. |
| `environment` | string ≤ 128 B | The id of the environment the task runs in, one of the `id` values of `environments` from `work.read`. Leave it out to keep the task's current environment, or the project's default when it has none. The host refuses an environment the project does not allow or one that lacks a capability in `needs`, and offers the task only to a node that can run it. |
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

### `aq.work.route_needed` 1.0.0

A task needs a route.

**Sent:** Today the host sends it on its own: for a ready task in a project on the automatic router that has no route; at once when the task changes to a new version, or is retried or reopened; again for the same version at most every two minutes while it stays unrouted, but not while a run started for that version is unfinished. After `not_offered` it is not sent again until the task, the active playbook or Settings changes; after `work.unroutable`, until the task or the active playbook changes.

Correlation fields: none. Kept 86400000 ms.

| Payload field | Type | Meaning |
| --- | --- | --- |
| `subject` | {id, revision} | The task's `{id, revision}`. Start the run with it as input. |

A trigger has no guard, so every emission starts a run.
