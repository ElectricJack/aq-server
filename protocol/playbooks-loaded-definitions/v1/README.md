# Loaded playbook definitions, version 1

Capability: `playbooks.loaded-definitions.read@1`. Profile:
`playbooks-loaded-definitions-1`. Route:
`GET /api/playbooks/inspection/loaded-definitions?project_id=<optional UUID>`.

This is an immutable, redacted record of artifacts actually loaded from the
configured runtime's startup deployments. `source` is always
`runtime_startup_configuration` and `current_catalog` is always `false`.
It does not report current installation, activation, authority, eligibility or
the complete persisted artifact catalog. A later installation does not change
this startup record. A new runtime startup captures its own configuration.

`project_id` is an exact namespace authorization/context guard, not a filter
over a project-installed inventory. The configuration loader binds each
deployment's project/workspace to its namespace. Matching that project does not
establish current installed or active state. The response states the namespace
and captures actual runtime configuration independently of the shared shell's
project selection.

The actual configured Driver must have captured this metadata, and the current
human must be both an Owner and the namespace owner. Authorization is checked
before and after the bounded read. Unconfigured routers have no route; a Driver
without captured configuration does not advertise the capability or substitute
an empty successful page. Foreign namespace/project context is refused.

At most four verified deployment artifacts are accepted by configuration.
Repeated identical artifact addresses are deduplicated and ordered by digest.
Only artifact/executable/source digests, name, compiler, purpose, entry names and
declared dependency artifact references are returned. Purpose is capped at 4096
UTF-8 bytes on a scalar boundary, with `purpose_truncated` set explicitly.
Declared dependencies are artifact declarations; actual run dependency pins
remain the separately retained admission fact. Raw source, registry schemas,
expressions, values, inputs/outputs, grant authority and configuration paths are
omitted. Text is literal, never HTML or Markdown execution.

Responses are at most 256 KiB, no-store, and use the existing two-slot,
three-second inspection read gate. Serving this metadata makes no engine
snapshot/catalog query and creates no mutation authority. Decimal namespace
generations stay strings.

The adjacent schema is generated from the public DTO by
`cargo run -p aq-server --example loaded_playbook_definitions_contract --locked --offline`.
Its SHA256 is
`8852409579266e5be4aa965ea4ca1af12c8820224d949db652c89c0503ac37ca`.
Objects are closed; clients additionally enforce source/current-catalog
semantics, UTF-8 byte bounds, scalar validity, exact scope and sorted identities.
