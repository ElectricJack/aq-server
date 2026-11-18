# playbooks-inspection-1

Human Owner/namespace-bound run, effect, receipt and admission discovery. Generated structural DTO schema: `cargo run -p aq-server --example playbook_inspection_contract`. Runtime bounds and version semantics are enforced by the wrapper and client validators; this schema does not replace authorization or live route qualification.

Playbooks view additions (G11, additive within `playbooks-inspection-1`): each run carries `definition` (definition `name`, AQ per-name install ordinal `version`, `pinned_is_active` and the `active` activation; null when unregistered, `pinned_is_active` null when the current catalog could not be read) and `waiting` (the pending human `decision_id`, its state and related task ids). Run detail adds `steps`, the pinned definition's step graph ordered breadth-first from the run's entry. The runs list accepts `since_ms` (inclusive) and `until_ms` (exclusive) on `admitted_at_ms`; a ranged page scans at most 8 engine pages and may return fewer items with a `next_cursor` that resumes the scan. Versions are recorded by `playbook_definitions` at install time; artifacts installed before migration 72 are registered on the next owner start. Each run also carries `binding` and `activation_revision` (decimal string): the binding and activation revision of the AQ admission that started it. Both are null for child runs, event-started runs, and admissions made before migration 90. `definition` prefers the activation of the run's own binding.

See [API-05 implementation boundary](../../../docs/implementation/API-05-playbooks-inspection.md). The profile reuses the frozen engine public inspector and does not modify any component archive or execution protocol.

The optional `playbooks.definition-graph.read@1` capability serves
`GET /api/playbooks/inspection/definitions/{artifact}/graph?project_id=<optional UUID>`.
The response has `api_version: "playbooks-definition-graph-1"`, the inspection
`namespace`, the exact artifact digest, and a `graph` with entry-to-start and
ID-to-step maps. Steps use the same public shape as run detail. The read
requires the mounted namespace owner and works independently of run history
and deployments. It serves the install-time projection, bounded at 64 KiB.
When there is no stored projection, `graph` is null. An installed artifact can
also include `description_markdown` from its matching retained authoring run
(up to 128 KiB when JSON encoded) and `step_facts`, a map from step IDs to
lists of `{label, value}` pairs (up to 32 KiB when JSON encoded). Facts describe
compiled behavior with names, modes, deadlines and outcomes. The graph and
facts exclude runtime input and output values, expression bodies, and captured
definitions. These fields are optional for definitions with no retained authoring
data or where a projection exceeds its size bound.

`step_inspection` optionally maps steps to a contextual `summary` and `uses`.
Each use names its `producer` and `consumer` step IDs, compiled field `paths`,
and optional `purpose`. The same use appears on both steps. These uses describe
data flow when the consumer runs. The projection uses the verified
retained artifact and reviewed command contracts. It omits facts it cannot
establish and is bounded at 24 KiB when JSON encoded. Clients can still use
the graph and step facts when this field is absent.
