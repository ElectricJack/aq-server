# Human Graph reads, version 1

`human-graph-1` is a distinct body contract from dashboard-read-1. The browser talks to AQ's human listener. AQ uses its installed service Graph client after resolving the current human Owner and exact project binding. An agent session ACL does not grant access here. No worker/agent credential is used or returned.

Routes:

- `GET /api/human/projects/{project}/tasks?limit=25&cursor=...&q=...`
- `GET /api/human/projects/{project}/tasks/{task}`
- `GET /api/human/projects/{project}/tasks/{task}/graph`
- `GET /api/human/projects/{project}/knowledge?limit=25&cursor=...&q=...`
- `GET /api/human/projects/{project}/knowledge/{node}?revision=1`

Every success has `{contract, project:{id,name,revision,workspace_id}, observed_at, data}`. Revision/version values are positive decimal strings. Page data is `{items,next_cursor}`; detail data is the explicit public node projection; graph data is `{root_id,nodes,edges,truncated}`. There are no total-count, live execution, readiness, acceptance or completion assertions. Node content/provenance is deliberately readable by the project Owner; arbitrary node metadata is excluded. Unknown configured facts are not synthesized.

Limits: list default 25 / max 64; q nonempty <=256 bytes and no controls; cursors <=4096 bytes. Authenticated encrypted cursors bind actor, project, project revision, workspace, kind, limit and search, expire after 30 seconds and become invalid after router restart. They do not grant access and never expose the upstream cursor. Knowledge history listing is deferred, while an exact content revision is supported.

Graph view is depth 1 with <=64 task nodes, <=256 retained edges and a 262144-byte upstream budget. It filters cross-kind nodes/edges and reports truncation when the neighborhood cannot be represented. Nodes and edges retain exact graph-owned revisions. Full response <=1 MiB. Two requests and a separate two-connection read pool bound this API; total handler timeout 5 seconds. Reads never hold database locks across Graph HTTP.

Ordinary errors use existing AQ `{error,message}` semantics: invalid bounds 400, unauthorized human 401/403, absent project/node 404, cursor/binding/authorization drift 409, unavailable/unsupported upstream 503. AQ rechecks the human role and project binding before response delivery. No automatic fallback to mock data or machine routes exists.

Integration requires mounting `agent_access::human::routes(&state)` inside AQ's protected human router, then publishing `human.graph.read`, `tasks.read`, `knowledge.read` v1 only for eligible Owner access with an installed Graph Runtime. Bootstrap wiring and the actual UI adapter must land together; this module alone does not enable those capabilities.
