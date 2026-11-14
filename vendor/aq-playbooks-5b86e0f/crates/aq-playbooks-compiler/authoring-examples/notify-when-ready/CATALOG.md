# Catalog

The commands and events this playbook may use, and what the host says about each. `aq-playbooks commands describe <name>` and `aq-playbooks events describe <name>` show the same contracts as JSON, with these notes. Where a meaning says "Not stated", the catalog does not say: do not rely on it, and ask the owner when the document depends on it.

## Commands

### `message.send` 1.0.0

Sends one message to a person.

A change, made once per call: a repeat of the same call returns the first result. The host answers within 60000 ms. Needs: `message.send`.

| Input field | Type | Meaning |
| --- | --- | --- |
| `body` | string ≤ 4096 B | The text, exactly as sent. A playbook cannot build text: use the document's own words, and ask when the document gives none. Today the host refuses an empty body, more than 1600 UTF-16 code units, or control characters other than newline and tab. |
| `identity` | string ≤ 4096 B | What the message is. Today the host derives the message from it: a second send with the same identity and the same body is answered `queued` again and sends nothing new; with a different body it is refused. So one identity is sent at most once, whatever the runs do. Use a value that names what the document says may be sent only once, such as a task's `reference` from `work.read` (one per task version). Today the host refuses an identity that is blank, longer than 512 bytes, or holds control characters. |
| `recipient` | string ≤ 4096 B | Who gets it. Take it from the facts, never from words of the document. Today the host delivers only to the namespace owner (`user:<id>`) and refuses any other recipient. |
| `visibility` | string ≤ 4096 B | Who may see it. Today the host accepts only `project`. |

| Outcome | Returns | When, and what next |
| --- | --- | --- |
| `queued` | {reference, subject} | The message is queued for delivery. Whether it was delivered or read is not reported. |
| `refused` | {reference, subject} | Nothing was sent. Today the host refuses a recipient or visibility it does not serve, a body out of bounds, or an identity already used with a different body. |

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

## Events

### `aq.work.changed` 1.0.0

A piece of work was ensured or changed.

**Sent:** Today the host sends it only as the effect of `work.ensure` (outcome `ensured`) or `work.change` (outcome `changed`) called by a playbook in the same namespace, any playbook. Never when a person creates, edits, finishes or cancels a task. `completed` and `cancelled` are in its schema, but today the host does not send them.

Correlation fields: `operation`. Kept 86400000 ms.

| Payload field | Type | Meaning |
| --- | --- | --- |
| `outcome` | one of `cancelled`, `changed`, `completed`, `ensured` | What the call did: `ensured` (the task exists now, whether or not the call created it) or `changed`. |
| `reference` | string ≤ 4096 B | The work's reference. |
| `subject` | {id, revision} | The work's `{id, revision}`. |

Its correlation field `operation` is the identity of the call that caused it.
