# C5a1 revision 2 review

Task: `solid-stone-20`. Review: `rev-quick-torrent`, revision 2.
Review document SHA256: `be7223a5b420b329c57d3e22cd679ece8c2bfa98906c357658c5a86fd5ef684d`.
Reviewed implementation: `c4afb9cebfb83baba235ec01435e0c8a55a52c72`.
The assigned review branch is based on a different commit; checks use a clean
archive of the pinned implementation, not the review branch's source.

## Findings

No blocking implementation defect found. One Low, nonblocking documentation
finding, posted as `cmt-144ecce2a31b`:

At the reviewed commit, `docs/specs/0004-intelligence-selection.md:46-49`
still requires existing installations to advance their configuration revision
once. That instruction conflicts with the compatibility fix and could cause
an unnecessary revision change that invalidates pinned preparations. Remove
the one-time migration instruction while retaining revision increases for
actual credential kind or backend-policy changes. Follow-up: `bright-willow-93`.
The supervisor was asked to place it under the package findings container.

## Prior findings

- `cmt-ce78f14d9edf`: resolved in implementation. Comparing
  `intelligence/registry.rs:6-39` with base `86ca279` shows that the untagged
  `Placement::Legacy` serializes the same two-element tuple. Unrestricted
  `AuthorizedCredential.allowed_backends` is omitted, including when the
  explicit full set is reordered. The rest of the digest input is unchanged.
  `register` returns without a revision change when that digest matches.
- `cmt-0e819d8ff46d`: resolved. `IntelligenceConfig::load` validates before
  sorting/deduplicating the derived policy. The digest omits the redundant raw
  allowlist. `BackendKind::ALL` drives the size/default checks; its schema test
  checks exhaustiveness and ordering.
- `cmt-b987560e0501`: resolved. Implementation documentation describes
  absent versus empty lists and the consequences of unknown backend identity
  until C4. Empty lists produce a load warning. Invalid lists fail validation
  before evaluation, while the policy helper also fails closed defensively.
- `cmt-d54ef7d85e27`: retained revision-2 clippy evidence reports a successful
  locked, all-target check for both changed crates with warnings denied.
  This review inspected that evidence; it does not claim a new clippy run.

## Additional source checks

Checked the C5a1 contract in
`docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml:1437-1508`.
Both credential kinds remain metadata, node restrictions remain independent,
and backend policy runs before delivery eligibility in `candidate_core.rs`.
Provisioning (`material.rs:144-150`) and issuance (`material/issuance.rs:38-54`)
check backend policy before private reads. Current server callers explicitly
supply `None`; advertisement consumption and permit/lineage/write-back work
remain outside this change. Selection tests cover explicit fallback and legacy
wire compatibility. The revision-2 retained server-area evidence reports 15
passing tests, including revision-change and issuance restrictions.

An independent Python SHA256 calculation of the pre-C5a1 canonical JSON tuple
reproduced the regression pin:
`35c9911673be2ce9dece9de54ea211f45be4b59f83f6b238e08a215cd0873ebd`.
The tuple contains revision 7, the legacy credential/node-list placement, no
harnesses, the loaded credential with `provisionable: false`, and a null
fingerprint. This also matches the retained base-version Rust probe.

## Independent execution

Commands ran through the retained pytest Cargo adapter under `aq test`, with
`CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, and one pytest adapter worker.
Reviewer logs are retained in `docs/implementation/evidence/C5a1-review-r2/`.

| Command on pinned archive | Result |
| --- | --- |
| `cargo test --locked -p aq-selection` | 43 passed: 1 unit, 15 central, 26 selection, 1 baseline; includes the named C5a1 acceptance test |
| `cargo test --locked -p aq-server --lib credential_` | 4 passed: legacy digest pin, kind/policy digest change, order-insensitive digest, and provisioning gate |
| `git diff --check 86ca279 c4afb9ce -- <reviewed Rust paths>` | Passed |

The first adapter attempt stopped before Cargo because its output directory
was absent. After creating it, the selection run passed. The first server
attempt reused an older slot artifact for `aq-selection` and reported missing
policy symbols despite matching source blobs. The archive source timestamps
were refreshed and only that package's Cargo artifacts were cleaned before
retrying. These setup failures are retained separately from successful results.
The retry compiled successfully and all four focused tests passed; no cache
errors remained. No implementation source was modified for these checks.

No live provider/backend execution or new full-suite run is claimed. The
review ran on the Codex/OpenAI harness; the revision identifies an Anthropic
author and has a matching Claude coauthor trailer. Exact author runtime model
telemetry was not independently available in the review response.

The cross-project gate-policy document referenced by prime was denied by the
session's file scope. This was reported once to the project supervisor as
`msg-11da356639e748faac8ee8f5325d9003`; no access bypass was attempted. The
review follows the explicit task and prime instructions and does not decide
the review gate.
