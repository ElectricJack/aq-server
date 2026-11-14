# AQ host catalog 1.0.0

`registry.json` is the authoritative executable schema artifact: the serialized
`aq_playbooks_model::Registry` with bounded `Schema` definitions. It contains
exactly twelve commands and seven events. `manifest.json` freezes the semantic
version, finite vocabularies and fingerprint. `fixtures.json` supplies valid,
invalid and compatibility vectors; stateful replay cases run in the fake host.
This data bundle has no path or repository dependencies. The CLI embeds the same
registry bytes rather than maintaining another catalog builder.

Vendor this directory from an exact aq-playbooks Git revision into aq-server's
pinned host source closure. Decode with the model shipped in the same revision,
using `serde_json::from_str::<Registry>(include_str!("registry.json"))`. No CLI or
fake host dependency is needed. Verify `manifest.fingerprint` using
`aq_playbooks_model::digest(&(version, &registry))`: SHA-256 over canonical JSON
of the pair `["1.0.0", registry]`, with sorted object keys and compact UTF-8.
Per-command and per-event fingerprints use the existing model canonical digest.
A changed executable schema requires a new version and fingerprint; existing
captured artifacts remain pinned and require deliberate rebinding.

Build and verify the owner independently:

```sh
cargo test --locked -p aq-playbooks-cli --test catalog_contract
cargo test --locked --workspace pilots_pass_on_reconciled_catalog
```

D5 must pin the exact source revision and manifest fingerprint, replace its
independent catalog definitions, admit the published recovery classes, and run
these fixtures through the production endpoint. A passing synthetic test does
not qualify production adapters. D3 and D8 consume the names and correlation
semantics documented in `docs/aq-host-integration.md`.
