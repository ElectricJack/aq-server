# AQ host catalog 2.1.0

This bundle makes the prepared candidate's immutable source object ID part of
the `candidate.prepare` contract, so a check binds the source the host actually
prepared instead of one the caller supplies. It sits beside
`contracts/aq-host/` (1.0.0) and `contracts/aq-host-2.0.0/`, which both stay
byte-frozen. The layout matches 2.0.0:
- `registry.json` is the serialized `aq_playbooks_model::Registry`;
- `manifest.json` holds the version, fingerprint and vocabularies, plus
  `supersedes` (2.0.0);
- `fixtures.json` holds the valid/invalid `cases` and a `compatibility` section.

The only change from 2.0.0 is `candidate.prepare`, now at command version 2.1.0:
- its `prepared` outcome requires `source: {format: sha1|sha256, hex}`, next to
  `reference` and `subject`. The schema is identical to `check.request` 2.0.0
  `input.source`;
- `source` joins `reference` and `subject` in `public_result_fields`;
- `conflict` and `refused` are unchanged and carry no source;
- the capability stays `candidate.prepare`, because the effect is unchanged.

The `compatibility` section pins:
- the 2.0.0 registry file SHA-256 and its manifest fingerprint;
- the fingerprints of the eleven commands and nine events that are
  byte-identical to 2.0.0;
- the one changed command (`candidate.prepare` 1.0.0 → 2.1.0);
- no added events.

Verify the fingerprint with `aq_playbooks_model::digest(&("2.1.0", &registry))`.

Vendor this directory as a new catalog profile in aq-server, for example
`vendor/aq-host-catalog-2.1.0/` (Host8), from an exact aq-playbooks revision.
Pin it to this manifest's fingerprint. Do not edit or re-fingerprint the 1.0.0
or 2.0.0 vendor copies. Then:
- the host's `candidate.prepare` adapter emits the prepared candidate's OID as
  `output.source`;
- policies bind `check.request` `source` to `candidate.source` (the prepare
  step's binding), not to a caller input.

See `docs/aq-host-integration.md` ("AQ host catalog 2.1.0") for details. To
verify:

```sh
cargo test --locked -p aq-playbooks-cli --test catalog_v2_1_contract
cargo test --locked -p aq-playbooks-cli --lib catalog_v2_1_check_binds_the_prepared_candidate_source
```
