# AQ host catalog 2.0.0

This bundle adds asynchronous provider judgment and registered check execution
for D6. It sits beside `contracts/aq-host/` (1.0.0), which stays byte-frozen.
The layout matches 1.0.0:
- `registry.json` is the serialized `aq_playbooks_model::Registry`;
- `manifest.json` holds the version, fingerprint and vocabularies, plus
  `supersedes`;
- `fixtures.json` holds the valid/invalid `cases` and the fake-host
  `replay_cases`, plus a `compatibility` section.

The `compatibility` section pins:
- the 1.0.0 registry file SHA-256 and its manifest fingerprint;
- the fingerprints of the nine commands and seven events that are byte-identical
  to 1.0.0;
- the three changed commands (judgment.request, check.request, operation.get →
  2.0.0);
- the two added events.

Verify the fingerprint with `aq_playbooks_model::digest(&("2.0.0", &registry))`.

Vendor this directory as a new catalog profile in aq-server, for example
`vendor/aq-host-catalog-2.0.0/`, from an exact aq-playbooks revision. Pin it to
this manifest's fingerprint. Do not edit or re-fingerprint the 1.0.0 vendor
copy. 1.0.0 namespaces and grants keep their bindings. The new effects require
the new grants `judgment.request.v2` and `check.request.v2`.

See `docs/aq-host-integration.md` ("AQ host catalog 2.0.0") for the contract
semantics and the compatibility decision. To verify:

```sh
cargo test --locked -p aq-playbooks-cli --test catalog_v2_contract
cargo test --locked -p aq-playbooks-cli --lib catalog_v2_async_judgment_and_check_compile_and_join
```
