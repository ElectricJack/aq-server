# AQ host catalog 2.4.0

Catalog 2.4.0 builds on the supported 2.2.0 lineage. The feature-gated delivery profile uses catalog 2.3.0.

`decision.request` includes the responder's `note` in approved and rejected results. The note is a string of up to 2048 UTF-8 bytes and is empty if the responder supplied none. The completion event and operation reconciliation contract carry it as well.

`work.activate` acts on an exact subject that the playbook namespace ensured. It atomically activates draft descendants and reports the numbers activated and skipped. A refusal gives the host's reason.

Regenerate the published bundle from the workspace root with `cargo test -p aq-playbooks-cli --test catalog_v2_4_contract write_catalog_v2_4 -- --ignored`.
