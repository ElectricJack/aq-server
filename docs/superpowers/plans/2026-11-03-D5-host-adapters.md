# D5 host adapters — TDD plan (crisp-meadow-36.6)

Goal: production `context.read`, `work.ensure`, `work.change` and `operation.cancel`
adapters in the embedded playbook host, bound to the pinned D4 catalog
(aq-playbooks `c9349a468bedb380282e085a7150b8f4c33752ed`,
`sha256:627125cc393d8274bfaa9ffc785c338e566cc69af362ac027a3daa355c3509e1`).

## Steps

1. **Vendor catalog** — `vendor/aq-host-catalog-1.0.0/` extracted with
   `git archive` from the pinned revision plus the source record
   `vendor/aq-host-catalog-1.0.0-source.json` (archive digest, per-file digests,
   no sibling paths).
2. **Failing tests first** — `crates/aq-server/tests/playbook_host_adapters.rs`:
   - `work_ensure_is_idempotent_by_key`: same logical key from two intents gives
     one graph create and the same task/handle; same intent id with other bytes is
     a conflict; an unknown kind is refused before any effect.
   - `work_change_conflict_returns_version_refusal`: a stale `subject.revision`
     returns the `conflict` outcome without any graph mutation; the current
     revision posts one `update_node` with `expected_version`.
   - `operation_cancel_reaches_native_attempt`: a server-owned attempt handle in
     the namespace project reaches B3 `profiled_execution::cancellation::cancel`
     and the outcome maps from its disposition (never_granted → `cancelled`);
     replay returns the retained result.
   - catalog test: the vendored registry decodes, matches the manifest
     fingerprint and every `fixtures.json` input/output/event case validates
     (or is refused) through the Host5 registry.
3. **Registry relaxation** — `aq-playbook-host::validate_registry` admits the
   contract's declared recovery classes (Observation, KeyedIdempotent,
   Reconcilable); `NaturallyIdempotent`/`Unsafe` stay refused.
4. **Catalog profile 3** (`CatalogProfile::Host5`) — registry is the vendored D4
   registry, verified against the fingerprint; scopes carry only
   `{context.read, work.ensure, work.change, operation.cancel}`; existing
   profile 1/2 namespaces and grants are never widened (namespace stamps are
   immutable and checked on every lock).
5. **Migration 0057** — catalog_profile 3, effect command/status vocabularies and
   `playbook_work_keys` (namespace, generation, logical key → task, handle,
   request digest).
6. **Adapters** (`playbooks/host_adapters.rs`) — one transaction per intent:
   retained lookup by intent id (identity digest covers namespace, intent id,
   contract fingerprint, payload digest, grant and revision; same id with other
   bytes → Conflict); live grant + intent authority; owner effect; effect row and
   outbox entry in the same transaction; grant expiry and deadline rechecked
   before commit. Graph access through a `WorkGraph` port whose production
   implementation is the agent-access graph gateway.
7. **Driver routing** — MailHost dispatches the four commands; reconcile only
   returns the retained effect.
8. Evidence in `docs/implementation/D5.md`.
