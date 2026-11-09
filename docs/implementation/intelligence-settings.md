# Intelligence Settings

The owner requested restoration of the v1 intelligence profile editor on 2026-11-09. Settings now links to `/settings/intelligence`, where the owner can inspect profiles, edit their display name and description, and choose configured provider/model/reasoning combinations. Model candidates can be added or removed. This increment edits existing profiles; creating profiles and provisioning new harness/model configurations are outside its scope.

The owner API is GET/POST `/api/settings/intelligence`, with GET `/api/settings/intelligence/operations/{id}` for save recovery. The existing owner and CSRF guards apply. Reads include referencing projects and qualified model choices, with no credential material. Catalog revisions are decimal strings in the new API.

Saving publishes new immutable intelligence and referring agent versions. Version allocation includes previously known identities, so rollback cannot cause a version number to be reused with changed bytes. One transaction stores the catalog, preserves exact execution-template pins, updates all project bindings referencing the old active catalog, records binding history, advances activation, and stores the operation receipt. Project enabled/disabled flags and permissions are preserved; running attempts keep their original artifacts. A failure rolls everything back. Concurrent stale editors receive a conflict, while exact operation replay returns the original receipt. A no-op save records a receipt without republishing or repinning.

Profile display metadata is optional and omitted from old definitions, preserving their exact artifact identities. There is no new migration or separate editable database catalog. The form captures its opened snapshot, preserves a conflicting draft and requires operation inspection or an exact retry after an unconfirmed response.

## Validation

- Existing `it_profiled` binary, profile catalog and inspection modules: 14 tests passed. Coverage includes actual owner routes and PostgreSQL, atomic publication/repinning, forced storage-failure rollback, concurrency, stale revisions, exact replay, unsupported choices, no-op saves, and the edited model/reasoning reaching future composition.
- `aq-profiles --tests`: 39 tests passed, including frozen schema/task compatibility, old golden artifact identities, compiler, registry and CLI tests.
- Frontend Intelligence and FrameworkStatus component checks: 9 tests passed, including conflict preservation and interrupted-save reconciliation/retry.
- Production dashboard build, mock exclusion, typecheck and server format/Clippy checks passed. Integration binaries remain at 81; the server library ratchet falls from 109103 to 109100 lines. The main editor/storage logic and extracted inspection/read helpers live in `aq-profile-settings`.
- Labelled fixture forms were captured at 1440, 390 and 320 pixels with no horizontal overflow or accessibility violations. Shared Settings tokens/panels govern this page; Settings has no separate approved PNG. Desktop and phone captures were inspected. This is the delivered profile-editor subset, not full-dashboard acceptance or independent code review.

Live evidence and rollback details are stored locally under `/home/jkern/dev/aq-platform/qa/2026-11-09/intelligence-settings/`. Live qualification uses an unchanged save; changed profile publication and future composition are qualified on the disposable test database without model calls.
