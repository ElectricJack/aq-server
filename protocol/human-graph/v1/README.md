# Human Graph reads, version 1

`human-graph-1` is a distinct body contract from dashboard-read-1. The browser talks to AQ's human listener. AQ uses its installed service Graph client after resolving the current human Owner and exact project binding. An agent session ACL does not grant access here. No worker/agent credential is used or returned.

Routes:

- `GET /api/human/projects/{project}/tasks?limit=25&cursor=...&q=...`
- `GET /api/human/projects/{project}/tasks/{task}`
- `GET /api/human/projects/{project}/tasks/{task}/graph?limit=63`
- `GET /api/human/projects/{project}/tasks/{task}/work`
- `GET /api/human/projects/{project}/tasks/{task}/history?cursor=...`
- `GET /api/human/projects/{project}/tasks/{task}/summary`
- `GET /api/human/projects/{project}/knowledge?limit=25&cursor=...&q=...`
- `GET /api/human/projects/{project}/knowledge/{node}?revision=1`
- `GET /api/human/projects/{project}/knowledge/{node}/relationships`
- `GET /api/human/projects/{project}/knowledge/{node}/revisions?limit=25&cursor=...`

Every success has `{contract, project:{id,name,revision,workspace_id}, observed_at, data}`. Revision/version values are positive decimal strings. Page data is `{items,next_cursor}`; detail data is the explicit public node projection; graph data is `{root_id,parent,nodes,edges,truncated,omitted,expansion}`. Task pages also carry `executions`. The first knowledge list page also carries a bounded filtered `total` (below); task pages carry no total. There are no acceptance or settlement assertions. Node content/provenance is deliberately readable by the project Owner; arbitrary node metadata is excluded. Unknown configured facts are not synthesized.

Limits: list default 25 / max 64; q nonempty <=256 bytes and no controls; cursors <=4096 bytes. Authenticated encrypted cursors bind actor, project, project revision, workspace, kind, limit and search, expire after 30 seconds and become invalid after router restart. They do not grant access and never expose the upstream cursor. An exact content revision is supported.

The first knowledge list page (no cursor) adds `total: {value, accuracy}` for the same `q` filter. Graph has no count endpoint, so AQ enumerates at most four 200-node upstream pages within 1.5 seconds: one complete page is `exact`, several pages are `estimated` (writes may land between pages), and an exceeded bound, timeout or upstream failure is `{value:null, accuracy:"unknown"}`. A count failure never fails the list and is never reported as zero. Later pages and task lists omit `total`.

Relationships data is `{root_id,nodes,edges,linked_work,counts,truncated}` for one knowledge node: live depth-1 edges incident to the node (either direction, <=256), the neighbour nodes of either kind they reach (<=64, root first), `linked_work` as the task neighbour ids, and `counts.relationships` / `counts.linked_work` as `{value, accuracy}`. Counts are `exact` only when the neighbourhood is fully represented; with `truncated` they are `unknown`. Edges to deleted nodes and deleted edges are omitted.

Revisions data is `{items,next_cursor}` of Graph-owned audit entries for one knowledge node, in Graph order. Each entry is `{sequence,entry_id,category,operation,actor_id,reason,change_set_id,created_at,revision,version,title,edge}`. `revision` and `title` are set for `revision` entries (open the content with `?revision=`), `edge` is `{id,edge_type,source,target}` for `edge` entries, and `version` is the node version when Graph records one. Bodies, metadata and raw snapshots are never returned. Limit default 25 / max 64; the cursor binds the node, and a page exceeding the 8 MiB upstream budget is 503 (use a smaller limit).

Graph view is depth 1 with <=64 task nodes, <=256 retained edges and a 262144-byte upstream budget. It filters cross-kind nodes/edges and reports truncation when the neighborhood cannot be represented. Nodes and edges retain exact graph-owned revisions. Full response <=1 MiB. Two requests and a separate two-connection read pool bound this API; total handler timeout 5 seconds. Reads never hold database locks across Graph HTTP.

Ordinary errors use existing AQ `{error,message}` semantics: invalid bounds 400, unauthorized human 401/403, absent project/node 404, cursor/binding/authorization drift 409, unavailable/unsupported upstream 503. AQ rechecks the human role and project binding before response delivery. No automatic fallback to mock data or machine routes exists.

Integration requires mounting `agent_access::human::routes(&state)` inside AQ's protected human router, then publishing `human.graph.read`, `tasks.read`, `knowledge.read`, `knowledge.relationships.read`, `knowledge.history.read` v1 only for eligible Owner access with an installed Graph Runtime. Bootstrap wiring and the actual UI adapter must land together; this module alone does not enable those capabilities.

## Work view projections (G11 X8-X13)

These additions serve the dashboard Work view. Counts are decimal strings.

- **Graph omitted counts and expansion (X10).** `graph?limit=N` (1-63, default 63) shows at most N neighbor tasks. `omitted` is `{tasks, tasks_by_state:{draft,active,completed,canceled,unknown}, other_nodes, edges, exact}`. It counts the neighbor tasks, cross-kind nodes and edges that AQ dropped. `exact` is false when the upstream Context itself was truncated: the counts are then lower bounds. `expansion` is `{limit}`, the next limit that would show the hidden tasks, or null when nothing more can be shown.
- **Parent hierarchy (X9).** `parent` on graph and work data comes from a live `parent-child` edge whose target is the task (the edge source is the parent). It is `{id,kind,title,task_state,edge_id}`, or null when the depth-1 Context holds no such edge. If the parent node is not in the Context, only `id` and `edge_id` are known.
- **Readiness and blockers (X8).** Work data is `{task_id,task_state,version,readiness:{state,blockers,complete},parent,execution}`. Blockers are live `blocks` edges that target the task. `satisfied` is true only for a `completed` source and null when the source is not in the Context. The state is the first match in this order:
  1. `draft`, `completed` or `canceled` from the task state.
  2. `running` when an attempt holds the task.
  3. `waiting` when any blocker is not satisfied.
  4. `unknown` when the Context was truncated.
  5. `ready` otherwise.
- **Attached execution (X12).** `execution`, and each entry of a task page's `executions`, is `{task_id,attempt_id,session_id,status}` for the `profiled_v1` attempt holding the task in this project. Pages list only tasks that have one.
- **History timeline (X13).** History data is `{task_id,items,next_cursor,attempts,attempts_truncated}`. `items` are Graph history entries (25 per page): sequence, entry id, category, operation, actor, reason and time, with the entry payload excluded. The cursor is the same encrypted, bound cursor that lists use. `attempts` holds up to 16 of the task's recorded attempts, with the holding attempt first. Validated result bodies and the latest result stay with the G6 Results feed (`GET /api/v1/results?task_id=`).
- **Per-scope completion (X11).** Summary data is `{task_id,scope:"descendants",implementation:{total,by_state,complete},delivery}`. It counts the live task descendants in a bounded Graph tree (depth 5, limit 200, 1 MiB). `complete` is false when any level was truncated. Delivery belongs to D1, and until D1 has a human read model it is reported as `{state:"unavailable",owner:"D1"}`.
