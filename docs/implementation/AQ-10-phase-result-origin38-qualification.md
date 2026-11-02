# AQ-10 B1 phase result-origin38 qualification

Source: `440298f` plus correction `3a1a4c1`, on accepted union `e510e7d`.
Plan: `939d251`; closure/v1 reference audit: `348f34a`.
Worktree: `/tmp/aq-phase-result-origin-v2`.

This qualifies the bounded server-side B1 claim-recording slice. It does not
qualify a physical host/proof producer, model/provider run, native changed-source
export, completed check, B2 settlement, verified release or Graph completion.
No canonical database/service/configuration was read, migrated or changed. No
production server binary was built. The unrelated wait-owner38 candidate remains
isolated and unmerged; this execution migration owns the next unreleased0038.

## Result and wire boundary

The existing worker POST and published `ResultReport`/Recorded receipt are
unchanged. The worker supplies the actual original stopped evidence nested in
its phase wrapper. The server independently resolves Legacy, Frozen or
RegistryRefresh historical issuance, registration, Prepared and full nominal
Started/Stopped owners, verifies exact canonical bytes/digests/proof/qualification,
and pins the outer origin. The nested stopped receipt keeps its original meaning;
no old DTO becomes phase authority.

The phase owner uses its actual nominal terminal qualification/proof producer;
the unrelated legacy `HostStopEvidenceV1` transport gate is required only by the
legacy recording path. Durable journal evidence remains required in both paths.
Current node authentication is rechecked under the node lock. Historical cleanup
and exact immutable replay can outlive execution expiry, cancellation and active
configuration retirement; none renew execution permission.

Recorded claims do not evaluate whether claimed checks passed, verify supplied
source/effect IDs, accept a result, release any hold, or update Graph. SQL38 still
unconditionally refuses phase verified-release insertion. Human Results discovery
and supplemental media remain explicitly legacy-only.

## Actual qualification

All PostgreSQL work used new disposable test databases through the existing test
support owner. Fixtures exercise actual phase publication/acceptance/issuance,
terminal and result SQL/service owners. Graph/Git/worker-installation observations
and host proof DTOs are explicitly synthetic; no physical runtime inference is
made from them.

| Check | Result |
| --- | --- |
| Frozen9 + RegistryRefresh9 actual-owner cases |18/18 PASS,0ignored,58.60s; cached build0.22s |
| Existing legacy result compatibility |7/7 PASS,7.684s; target compile4.29s |
| Reviewed fresh37→38 catalog/startup and drift cases |2/2 PASS,1.15s; libtest rebuild1m38s |
| Affected strict Clippy, all features, lib + both test targets |PASS,26.83s |
| Actual populated archive37→38 follow-up |1/1 PASS,0.71s; dedicated target compile1.88s, strict0.39s |
| Affected Rust formatting |PASS,11files; `git diff --check` clean |

The18 cases cover each nominal profile separately: actual record/exact replay/
historical read with untouched authority/holds/effects/release state; expired and
cancelled history after active runtime configuration retirement; changed immutable
operation bytes and competing correction heads; legacy-only human coverage and
actual issuance FK refusal; privileged corruption of retained qualification/proof
with restoration; missing writer marker and unconditional release refusal;
observed node-lock wait followed by credential revocation; absent actual stopped
owner; injected failure between report/head and successful exact retry after the
whole failed transaction rolled back.

The seven selected existing regressions were:

- `recording_retains_claims_replays_and_serializes_corrections_without_settlement`
- `report_quota_preserves_exact_replay_and_immutable_heads`
- `sql_requires_exact_stopped_outcome_and_atomic_report_head`
- `internal_owner_read_is_historical_exact_and_marks_a_superseded_head_without_eligibility`
- `human_results::human_metadata_projects_exact_revisions_corrections_and_bounded_pages`
- `human_results::stopped_without_report_is_empty_and_current_viewer_cannot_read_results`
- `media_http::actual_http_reserves_replays_retains_lists_and_stream_frames_hold_all_guards`

Schema cases verify the reviewed78object reference, representative old global
conversation identity preservation, older37 reader refusal, and startup refusal
for same-name CHECK replacement, missing origin FK, dropped required nullability,
disabled named/all triggers and replaced guard function. This is not a complete
production-data migration/restore rehearsal. Frozen migrations1–37 are unchanged.

DDL SHA256:
`152e6f473e353d103cc850121ffd9bd3dabb9d37cc4d3dd2fa2e6a7822053297`.
Reviewed catalog JSON SHA256:
`7b42bc8eb84accf135bc9a4c7f46ab3cae2a563dcf01b7b7d271a8dd7385b8b5`.
The reference was explicitly exported from a fresh37 prefix followed by the exact
reviewed38 DDL; normal startup only compares it and cannot regenerate approval.

Representative commands used the existing shared target, two Cargo jobs,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_INCREMENTAL=0` and offline dependencies:

```sh
cargo test --offline -p aq-server --test phase_results -- --test-threads=1
cargo test --offline -p aq-server --lib store::result38_tests -- --test-threads=1
cargo clippy --offline -p aq-server --all-features --lib \
  --test phase_results --test profiled_results -- -D warnings
```

The selected legacy cases ran by exact name using the qualified `profiled_results`
test executable, after one `cargo test --test profiled_results --no-run` compile.
No entire workspace or server test suite was run.

## Failures retained and corrected

The first18case invocation failed in Store startup because the new retained-origin
consistency SELECT had one extra closing parenthesis. The earlier explicit DDL/
catalog-export case did not call that startup query. After correction, both actual
schema/startup cases passed.

The next18case invocation passed four negative guards but failed fourteen
report-positive paths. A single bounded diagnostic run isolated an incorrectly
copied legacy HostStop feature requirement after the actual phase context,
terminal and row checks had passed. Removing only that unrelated phase gate
preserved the actual phase owner qualification and legacy guard. Both positive
profiles then passed (2/2,6.84s after45.99s compile), followed by the full18case pass.
Temporary diagnostics were removed before the qualified source freeze.

Independent read-only review by Playbooks accepted `440298f` and separately the
three-file correction `3a1a4c1`; it did not run builds/SQL or claim physical proof.

Local nonsecret logs:

- `/tmp/aq-result38-catalog-export.log`
- `/tmp/aq-result38-phase-tests-first-startup-negative.log`
- `/tmp/aq-result38-schema-phase-tests.log` (2schema; phase target was filtered)
- `/tmp/aq-result38-phase-tests-corrected.log` (four passed/fourteen failed)
- `/tmp/aq-result38-origin-diagnostic.log`
- `/tmp/aq-result38-origin-corrected-positives.log`
- `/tmp/aq-result38-phase-tests-final.log`
- `/tmp/aq-result38-legacy-compile.log`
- `/tmp/aq-result38-legacy-tests.log`
- `/tmp/aq-result38-strict.log`

The shared SERVER target was explicitly returned to Root after strict completion.
No executables, backups or source artifacts were deleted by this lane. Public
phase Results coverage, agent-facing producer integration, B2/C and any canonical
schema activation remain separate integration gates.


## Populated archive37→38 follow-up

The fresh-schema case above did not prove populated legacy result preservation.
Root required this separate actual retained-data qualification before main38
integration. The immutable `/tmp/aq-retained36-37-proof/before.dump` is a508147byte
state36 archive with an actual legacy result and two actual retained PNG receipts,
SHA256 `c3f6f2e310f64ec60214de4c74eb27fd17ddfa0dff04c6bd712bdc29f84ad93d`.
It was restored only into a new explicitly owned private rehearsal database. The
original database/archive/media roots were not changed. No media owner/runtime
was activated and no upload occurred.

`tests/result_origin_migration.rs` is ignored by default and requires an explicit
owned-clone URL (restricted name prefix+UUID) and a new private0700 evidence
directory. It uses the existing `test_support::migrate_profile` wrapper around the
actual production migration/guard implementation. It applies37 if needed, then
captures every original table through its exact captured37columns and performs
real38 migration/startup validation. It does not stamp schema versions or bypass
guards.

Qualified preservation:

- All116original table bags across910old columns, their original column
  definitions and all5sequence states matched exactly.
- Only the explicit format metadata change37→38 and additional migration38ledger
  row were excluded from old-row bag comparison; both were independently checked.
- The actual legacy report and its Recorded receipt were compared as raw byte
  vectors before/after migration, as were both retained PNG receipts.
- Every original result received exactly `legacy_prepare_v1` with NULL phase
  pins/generated phase attempt. No receipt or source identity was relabeled.
- Private before37/after38 manifests were byte-identical, SHA256
  `70f4d499bfe6419548e87c43a5808844f8fda81071940719bf24afa91d72dbf3`.

Actual pass:1case in0.71s after1.88s dedicated integration-target compile.
Affected all-feature strict Clippy for lib+that target passed0.39s; formatting and
`git diff --check` passed. The initial unit-target attempt took1m27s compile and
caught a missing table alias in the new snapshot helper before capturing data or
applying38; the already completed real36→37 migration was retained honestly. The
helper was corrected and moved to its dedicated integration target, using the
existing public test-support migration seam. No production code/DDL changed in
this follow-up.

Private0600 artifacts are under `/tmp/aq-result38-populated-proof`: `clone.json`,
`before37.json`, `after38.json`, `proof.json`, `restore.log`,
`first-alias-negative.log`, `test.log`, and `strict.log`. The owned clone remains
available for Root inspection. Existing six-file/root identity evidence belongs
to the earlier36→37 proof; this SQL-only follow-up deliberately did not touch
those files. SERVER was explicitly returned to Root immediately after strict.
