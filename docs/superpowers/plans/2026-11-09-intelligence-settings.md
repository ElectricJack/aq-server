# Intelligence Settings MVP

**Owner scope:** restore viewing and editing intelligence profiles in Settings: names, descriptions, provider/model and reasoning choices. Edits apply to future launches; running sessions retain their settings. Requested 2026-11-09 after the dashboard cleanup. Owner followup: use a spreadsheet with Add row and a top-level provider selector; make every Settings section a tab.

**Existing design:** architecture 0003 §2.1–2.2 defines immutable activated catalogs, retained launch artifacts and revision-checked project bindings. This feature adds an owner editor to that existing publication path.

**Implementation calls:**

- Store optional display name/description in the versioned intelligence definition, omitted when absent to preserve all prior artifact identities. Publish new intelligence and referring agent versions, choosing numbers above every known identity, including after rollback.
- Save the catalog, exact existing environment pins, project references and binding history in one PostgreSQL transaction. Keep project enabled/disabled flags, permissions, source policies and all old artifacts. Take the existing project-writer gate before the catalog lock to maintain the established lock order.
- Offer configured qualified harness model/reasoning tuples. Keep existing unconfigured candidates visible and allow label-only edits, but reject introducing an unconfigured execution choice. Provisioning new providers/harnesses remains separate.
- Keep the server library within its ratchet by moving deterministic inspection and catalog read helpers into the new `aq-profile-settings` crate with the editor's transformation/storage code.

## Files and interfaces

| Area | Files / interface |
| --- | --- |
| Definition metadata | `crates/aq-profiles/src/catalog.rs`: optional bounded `name` and `description` |
| Transformation and atomic storage | `crates/aq-profile-settings/src/{lib,store}.rs` |
| Existing deterministic inspections | `crates/aq-profile-settings/src/inspection.rs`; thin existing server wrapper |
| Owner routes | `crates/aq-server/src/profile_catalog/settings.rs`: GET/POST `/api/settings/intelligence`, GET `/api/settings/intelligence/operations/{id}` |
| Reads | string catalog revision, active digest, current intelligence definitions, referencing projects, configured model/reasoning choices |
| Saves | operation UUID, expected string catalog revision, exact profile ID/version, name, description and candidates; receipt with disposition, revision, published profile and updated project IDs |
| Frontend | aq-dashboard `src/api/intelligenceSettings.ts`, `src/features/settings/{intelligence.tsx,Intelligence.module.css}`, Settings tabs, `/settings?tab=intelligence` and legacy route redirect |

No new database migration or integration test binary is needed. New routes use the existing owner authentication and CSRF guard. Operation receipts permit exact retries and read-only recovery after an interrupted response. Each edited row owns its profile-version snapshot, preserves a conflicting draft and does not replace it during background refresh or tab/provider changes. Version zero creates a new row; empty candidates yield a deliberate unconfigured placeholder. Creation does not add agent/project assignments.

## Verification

- Extend existing `it_profiled::profile_catalog` coverage for atomic repinning, retained old artifacts, immutable versions, stale/concurrent editors, idempotent replay, rollback on binding-storage failure, unconfigured-model refusal, no-op saves and the edited model/reasoning reaching future composition.
- Re-run existing profile inspection and activation tests after extracting helpers; check `aq-profiles` library tests.
- Frontend component coverage: successful save, conflict preservation, interrupted-save inspection, exact retry and Settings navigation, provider preservation, adding rows and tab drafts.
- Inspect labelled fixture and actual compiled live pages at desktop, 390px and 320px; check keyboard controls, overflow and accessibility. Confirm the exact compiled bundle is served.
- Qualify a changed profile and project binding against a disposable database. On the owner's stack use a no-op UI save so qualification does not change provider/model choices or launch agents.

## Budgets and delivery

| Budget | Before | After |
| --- | ---: | ---: |
| Integration test binaries | 81 | 81 |
| aq-server library lines | 109103 | 109100 |

Each worktree uses its own Cargo target. Tests run under `timeout`. Record tests, source and rollback evidence, publish draft PRs, and leave merges subject to independent review. Deploy the requested local change with a rollback copy and verify worker heartbeats continue. No v1 process or checkout is changed.
