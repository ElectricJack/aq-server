# dashboard-read-1 backend qualification

Frozen source: `3632eada81389f2dd9d0ab3a1f148c78746c9a42`. Five actual disposable
PostgreSQL/authenticated-router tests and two pure cursor tests pass. Strict
library/owned-test/schema-example Clippy, format, schema generation drift and all
five retained response schema checks pass. Exact source and retained log hashes
are recorded in `dashboard-read-1-evidence.json`.

The router tests cover nonempty sealed native-shaped SQL fixtures, owner and
Viewer ACLs, current revocation, expiry during a held ACL query, wrong boot
candidate exclusion, visibility overflow, project scope refusal, bounded input,
secret canaries, unknown process/count facts and structured malformed/auth errors.
Cursor tests cover valid resume, principal/auth partition/scope/resource/limit,
exact expiry, ciphertext tamper, restart key, eviction, and 256/64/4 quotas.
Independent read-only review found and prompted the post-await expiry and
current-user/ordered-ACL share-lock fences before this freeze.

The fixtures are synthetic and cannot qualify native launch or provider activity.
No migration, browser mutation, provider call, deployment or production service
was used. Backing-table scan performance is not qualified by returned-row limits.
Actual combined daemon/browser smoke and full assembled checks are separate
parent-owned evidence; these results cover this backend subset only.
