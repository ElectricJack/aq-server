# Current namespace catalog read profile 1

The component-owned AQ adapter is specified in
[API-05 current catalog](../../../docs/implementation/API-05-current-catalog.md).
`examples.json` contains schematic closed DTO examples, not actual installation
or activation evidence. The schema generator is
`cargo run -p aq-server --example current_playbook_catalog_contract` and will be
retained as `schema.json` after the assigned backend build gate. Source interface
is `playbooks::inspection::catalog::{CatalogQuery, CatalogPage, Kind}`.

The two kinds have separate fixed read routes. Continuations join exact namespace,
kind, owner incarnation and metadata revision. Every revision is a decimal
string. Current catalog capability is separate from the unchanged historical
loaded-startup profile. No mutation, execution authority or full-artifact field
exists in this profile.
