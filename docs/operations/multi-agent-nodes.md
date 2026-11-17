# Multi-agent worker nodes

The hierarchy is machine → worker nodes → slots. Keep Local Worker 1 and 2 on
this desktop and Local Worker 3 on the laptop. Each node has its own credential,
service boot, and slot count; two nodes configured for three slots can run six
agents on their shared machine, subject to machine and provider ceilings.

Upgrade the server (state format 135) first, then the slot-aware worker. The
server answers control schema 1 without new fields. Schema 2 includes `slots`.
An old worker remains limited to native slot 1 even if its Setting is raised.
No running processes, enrollment files or live services are changed by this note.

Read `GET /api/worker-management` for settings and their revisions. Change one
node with `PUT /api/worker-management/node/{node-id}` (POST is also supported):

```json
{"operation_id":"<new UUID>","expected_revision":1,"label":null,"max_agents":3}
```

Use the revision actually returned by the read, preserve any label you want to
keep, and retry a lost response with the same operation id and body. A conflicting
revision requires a fresh read. `max_agents` is the one shared count for queued
attempts and manual sessions, from zero to 1024. Zero pauses admissions. Lowering
it lets running agents finish, including those whose index exceeds the new count.
Unknown attempts and manual stops still pending retain their slots. A task waiting
for settlement after proven stop can remain held without consuming compute.

The same API at `/api/worker-management/default/node-slots` changes the default
for future enrollments. Supply its `node_slot_default.revision` and `max_agents`.
Migration 135 sets existing nodes to one (explicit zero remains zero), including
nodes that previously allowed additional manual sessions. New nodes snapshot the
server default, initially two. Editing the default does not resize existing nodes.

Retire Local Worker 4 through 9 only after the worker has recovered each old
identity's journal with that identity's credential. Mark them removed through the
node management API with `remove:true`, retaining their current label, count and
revision. This prevents new work and requests handoff for remaining attempts;
it does not fabricate stop proof, erase history or revoke the other desktop node.
Do not delete old local state or credentials until every outstanding attempt is
individually resolved. The worker must preserve separate journals, workspaces,
Git roots/sockets, session hosts and sandbox names for its concurrent slots.

Worker-side qualification must demonstrate physical isolation, node-wide inventory
serialization, shared admission for manual and queued execution, shrinking without
killing agents, and recovery of every previous boot's attempt. Server fixture tests
alone do not certify those physical properties.
