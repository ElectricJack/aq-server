---
name: aq-playbooks-and-gates
description: Inspect AQ v2 playbook catalog, admissions, runs, and decision gates, or make authorized owner changes through the command registry.
---

# AQ v2 playbooks and gates

Use the owner connection flags in `aq-cli`. Check `aq commands --all` for
available operations and use the server's returned IDs and revisions.

```sh
aq --json playbook status
aq playbook source list
aq playbook source show --source-id "$source_id"
aq playbook catalog activations --project-id "$project_id"
aq playbook inspection runs --project-id "$project_id"
aq playbook inspection run --run-id "$run_id" --project-id "$project_id"
aq playbook admission show --request-id "$request_id"
```

Read the binding, active artifact, activation revision, entry, and input
contract before an authorized manual start. Use `aq playbook run start --help`
for the arguments and keep the returned admission or run ID. If admission is
uncertain, inspect it and use the lookup-only `playbook admission recover`
when appropriate. Retrying an unknown admission may execute work twice.
Inspect its evidence before deciding whether a retry is authorized.

An open review decision carries its own subject, revision and authorized
decider. Follow `aq-reviews` for that decision. A question to the owner uses
`aq-comms`. Resolve the decision or answer the question through its own
surface, then inspect the run to see how the binding handled it.

To author playbook source, use the authoring workflow from the `aq-playbooks`
repository. In AQ, inspect `playbook source register --help` and
`playbook source edit --help` before submitting an authorized change.
Registration or editing starts authoring. Read the source status and active
artifact after submission. Activation and rollback change live policy and
require the corresponding authorization. The deployed bindings and routing
settings determine which pipeline applies.
