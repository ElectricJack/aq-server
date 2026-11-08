# Human Graph reads, version 1

`human-graph-1` is a distinct body contract from dashboard-read-1. The browser talks to AQ's human listener. AQ uses its installed service Graph client after resolving the current human Owner and exact project binding. An agent session ACL does not grant access here. No worker/agent credential is used or returned.

Routes:

- `GET /api/human/projects/{project}/tasks?limit=25&cursor=...&q=...`
- `GET /api/human/projects/{project}/tasks/{task}`
- `GET /api/human/projects/{project}/tasks/{task}/graph?limit=63`
- `GET /api/human/projects/{project}/tasks/{task}/work`
- `GET /api/human/projects/{project}/tasks/{task}/history?cursor=...`
- `GET /api/human/projects/{project}/tasks/{task}/summary`
- `GET /api/human/projects/{project}/tasks/{task}/comments?limit=50&cursor=...`
- `GET /api/human/projects/{project}/knowledge?limit=25&cursor=...&q=...`
- `GET /api/human/projects/{project}/knowledge/{node}?revision=1`

Every success has `{contract, project:{id,name,revision,workspace_id}, observed_at, data}`. Revision/version values are positive decimal strings. Page data is `{items,next_cursor}`; detail data is the explicit public node projection; graph data is `{root_id,parent,nodes,edges,truncated,omitted,expansion}`. Task pages also carry `executions`. There are no acceptance or settlement assertions, and there is no project-wide total count. Node content/provenance is deliberately readable by the project Owner; arbitrary node metadata is excluded. Unknown configured facts are not synthesized. Graph attributes every AQ write to AQ's one mutation identity; where `created_by` or `last_verified_by` is that identity and AQ's own operation journal names the owner whose dashboard operation created (operation ID = node ID) or last verified the record, the field carries that owner's email instead.

Limits: list default 25 / max 64; q nonempty <=256 bytes and no controls; cursors <=4096 bytes. Authenticated encrypted cursors bind actor, project, project revision, workspace, kind, limit and search, expire after 30 seconds and become invalid after router restart. They do not grant access and never expose the upstream cursor. Knowledge history listing is deferred, while an exact content revision is supported.

Graph view is depth 1 with <=64 task nodes, <=256 retained edges and a 262144-byte upstream budget. It filters cross-kind nodes/edges and reports truncation when the neighborhood cannot be represented. Nodes and edges retain exact graph-owned revisions. Full response <=1 MiB. Four running requests (twelve more wait up to 2 seconds, then 503) and a separate four-connection read pool bound this API; total handler timeout 5 seconds. Reads never hold database locks across Graph HTTP.

Ordinary errors use existing AQ `{error,message}` semantics: invalid bounds 400, unauthorized human 401/403, absent project/node 404, cursor/binding/authorization drift 409, unavailable/unsupported upstream 503. AQ rechecks the human role and project binding before response delivery. No automatic fallback to mock data or machine routes exists.

Integration requires mounting `agent_access::human::routes(&state)` inside AQ's protected human router, then publishing `human.graph.read`, `tasks.read`, `knowledge.read` v1 only for eligible Owner access with an installed Graph Runtime. Bootstrap wiring and the actual UI adapter must land together; this module alone does not enable those capabilities.

## Work view projections (G11 X8-X13)

These additions serve the dashboard Work view. Counts are decimal strings.

- **Graph omitted counts and expansion (X10).** `graph?limit=N` (1-63, default 63) shows at most N neighbor tasks. `omitted` is `{tasks, tasks_by_state:{draft,active,completed,canceled,unknown}, other_nodes, edges, exact}`. It counts the neighbor tasks, cross-kind nodes and edges that AQ dropped. `exact` is false when the upstream Context itself was truncated: the counts are then lower bounds. `expansion` is `{limit}`, the next limit that would show the hidden tasks, or null when nothing more can be shown.
- **Parent hierarchy (X9).** `parent` on graph and work data comes from a live `parent-child` edge whose target is the task (the edge source is the parent). It is `{id,kind,title,task_state,edge_id}`, or null when the depth-1 Context holds no such edge. If the parent node is not in the Context, only `id` and `edge_id` are known.
- **Readiness and blockers (X8).** Work data is `{task_id,task_state,version,readiness:{state,blockers,complete,reasons},parent,execution}`. Blockers are live `blocks` edges that target the task. `satisfied` is true only for a `completed` source and null when the source is not in the Context. The state is the first match in this order:
  1. `draft`, `completed` or `canceled` from the task state.
  2. `running` when an attempt holds the task and is still executing: its authority status is not `finished`, `declined` or `never_granted`, and it has no Stopped or NotStarted proof.
  3. `held` when an attempt holds the task but has stopped or finished, or when an active task has a `task_held` hold (below). The task is not running and no new attempt starts until the claim is released or the hold clears.
  4. `waiting` when any blocker is not satisfied.
  5. `unknown` when the Context was truncated.
  6. `ready` otherwise.

  `reasons` (at most 4, empty unless a stopped attempt holds the task, in any state, or the active task has a `task_held` hold) explains a hold as `{code,attempt_id,status,message}` (plus `hold_reason` for `task_held`). `code` is one of:
  - `cancelled_attempt_holds_task`: the attempt was cancelled (B3) and has stopped. A cancelled attempt keeps its retained source for a replacement (B4), so it keeps the claim until the task is retried or the owner abandons the attempt. This is a known limitation.
  - `attempt_stopped_not_released`: the attempt finished and stopped; the claim stays until its result is settled, the task is retried or the attempt is abandoned.
  - `attempt_finished_stop_unconfirmed`: the attempt finished but no stop proof is recorded yet.
  - `task_held` (R6, added 2026-11-08): AQ does not offer this active task, because a server-side hold was recorded on it: an exhausted retry, time or cost budget (`lineage_retry_budget_exhausted`, `replacement_policy_retry_budget_exhausted`, ...), a lost attempt whose retry was not admitted, or a successor refused on 20 polls in a row (`handoff_successor_stuck`, `replacement_successor_stuck`; AQ keeps trying those). `hold_reason` carries the server reason code (only on this code), `attempt_id` and `status` the attempt the hold is about, and `message` a fixed phrase for the code. The state is `held`. The hold lasts until the task has a new attempt; the same hold is named once in a system comment on the task. A reader that does not know `task_held` should show the task as not running.

  `held` and `reasons` were added on 2026-11-06 (QA P1-01). A reader that does not know `held` should show the task as not running.
- **Attached execution (X12).** `execution`, and each entry of a task page's `executions`, is `{task_id,attempt_id,session_id,status}` for the `profiled_v1` attempt holding the task in this project. Pages list only tasks that have one.
- **History timeline (X13).** History data is `{task_id,items,next_cursor,attempts,attempts_truncated}`. `items` are Graph history entries (25 per page): sequence, entry id, category, operation, actor, reason, time and `change`, with the entry payload excluded.
  - **Actor.** Graph records AQ's one mutation identity as the actor of every AQ write. When an owner's journalled operation confirmed the entry's Graph change set (a dashboard action, or a human task create), or the entry is the creation of a task whose ID is an owner's operation ID, `actor_id` carries that owner's email instead. Other actors are unchanged.
  - **Change.** `change` is null when AQ cannot describe the entry, otherwise `{summary,fields,edge,complete}`, derived from the Graph payload. `summary` is a short text (at most 512 characters), such as `active → completed`, `priority 0 → 5; title` or `added blocks from 3f2a1b9c`. `fields` lists `{field,from,to}` for each changed field (`title`, `body`, `acceptance_criteria`, `labels`, `priority` for metadata `aq:priority`, `metadata` for any other key, `task_state`, `provenance`, `content_type`, `retired`, `retirement_reason`, `successor_id`, `valid_from`, `valid_until`, `review_at`, `deleted`). `from` and `to` are strings for task state, priority, content type, retired and deleted, and null otherwise, so record text never appears there. Lifecycle entries give the state `from` and `to`. Edge entries set `edge` to `{edge_type,direction,node_id}`: `incoming` when the task is the edge target (for `blocks`, the task is blocked by `node_id`), `outgoing` when it is the source. Revision entries compare the snapshot with the task's previous revision: from the same page, or read from Graph (`/revisions/{n}`) for the page's oldest one. When that read fails, `complete` is false and `fields` is empty. The cursor is the same encrypted, bound cursor that lists use. `attempts` holds up to 16 of the task's recorded attempts, with the holding attempt first. Validated result bodies and the latest result stay with the G6 Results feed (`GET /api/v1/results?task_id=`).
- **Per-scope completion (X11).** Summary data is `{task_id,scope:"descendants",implementation:{total,by_state,complete},delivery}`. It counts the live task descendants in a bounded Graph tree (depth 5, limit 200, 1 MiB). `complete` is false when any level was truncated. Delivery belongs to D1, and until D1 has a human read model it is reported as `{state:"unavailable",owner:"D1"}`.

## Task comments (aq-dashboard spec 0004 §4.1)

`GET /api/human/projects/{project}/tasks/{task}/comments?limit=N&cursor=...` reads the task's comment thread. Comments are an aq-server model keyed by `(project_id, task_id)`, not Graph content (`crates/aq-task-comments`, table `task_comments`). Bootstrap advertises `tasks.comments.read@1` (owner with a configured Graph runtime) and `tasks.comments.write@1` (also requires dashboard actions).

- Data is `HumanComments`: `{task_id, items, next_cursor}`. `items` are `HumanComment`s, newest first (insertion order). `limit` defaults to 50 and is 1-100; anything else is 400.
- A comment is `{comment_id, task_id, author, body, created_at, edited_at, deleted_at, revision, can_edit}`.
  - `author` is `{kind, user_id, display_name, session_id, attempt_id}`, with every key present. `kind` is `owner`, `agent`, `supervisor` or `system`. Owners carry `user_id` and their current email as `display_name`. Agents carry `session_id` and `attempt_id` (link `/agents/{session_id}`). Supervisors carry `session_id`; system authors carry their component name as `display_name`.
  - `body` is Markdown, at most 16 KiB. A deleted comment is a tombstone that keeps its position: `body` is null and `deleted_at` is set.
  - `revision` is a positive decimal string. Edit and delete target it exactly.
  - `can_edit` is true only for the requesting owner's own live comments. Owners can edit or delete only comments they wrote.
- `next_cursor` is the same encrypted, bound cursor as lists (actor, project, project revision, workspace, task and limit). Because comment pages are keyset positions, a comment cursor expires after 10 minutes instead of 30 seconds; it still becomes invalid after a server restart or project binding change (409: restart from the first page).
- The read does not consult Graph: an unknown task returns an empty thread.
- Writes are `dashboard-actions-1` kinds `task.comment.add`, `task.comment.edit` and `task.comment.delete` (see that contract). Agents add and read their own task's thread through `protocol/agent-comments/v1`.
