# C5b: versioned subscription login with compare-and-swap write-back

Task: `calm-delta-18.8`. Branch: `aq/calm-delta-18.8`.
Plan: [2026-11-04-c5b-subscription-refresh](../superpowers/plans/2026-11-04-c5b-subscription-refresh.md). The owner rescope below replaces that plan's lease-limit, enrollment and lineage-fencing tasks.

## Owner decisions (2026-11-04)

- **Claude first.** The Claude Code path comes first. Codex usage is exhausted until about 2026-11-08, so Codex is not on the critical path.
- **Claude tokens need no write-back.** The operator's `claude setup-token` spike found the Claude token is a long-lived, env-only bearer (`CLAUDE_CODE_OAUTH_TOKEN`) that the CLI never rewrites. Claude subscription logins are therefore configured as env-only leases. They are stored and versioned like any other login.
- **Why write-back exists.** It is for logins the CLI rewrites in place, such as Codex `auth.json` after a token refresh.
- **Rescope.** The owner rescoped C5b to the simple design:
  1. One stored login per credential, with a version.
  2. An operator import/replace that always wins.
  3. A compare-and-swap write-back.
  4. Renewal compares credential identity, not bytes.
  5. The next attempt fetches the newest version.
- **Deferred.** Codex device-auth enrollment, `max_concurrent_leases`/`refresh_safe`, and lineage fencing beyond the version CAS are dropped from this task and deferred. See [Follow-ups](#follow-ups).

## Design

### Versioned login

- **Credential key.** A credential's login is keyed by three values:
  - `identity` = `profile.id@profile.version`
  - the registry credential revision
  - `base`, the startup sha256 fingerprint of the configured `secret_file`
- **Versions.** The configured file is **version 0**. Each stored entry in `credential_material_revisions` (migration 0088) has these fields:
  - `sequence`, which is the version
  - `origin`: `refresh` (a node write-back, `lease_id` required) or `operator` (an import/replace, `lease_id` forbidden)
  - `content_digest`
  - `content_bytes`
- **No secret bytes in SQL.** SQL holds digests and sizes only.
- **Newest version.** The newest version is the highest `sequence`, or the configured file when there are no entries.
- **Rotating the configured file.** Replacing the configured file and reloading changes `base`. This starts a fresh history at version 0.

### Store

- **Location.** Bytes live under `[intelligence] material_store`, which must be an absolute path. The layout follows the existing owner-only file pattern:
  - one `<revision_id>.material` file per version
  - opened with `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600
  - inside an owner-uid 0700 directory
- **Write order.** The file is fsynced before its row commits. A crash can leave an orphan file, but never a row that points at missing bytes.
- **Reads.** Reads use `O_NOFOLLOW` and check that the file is owner-only, that its length matches, and that its sha256 matches. A mismatch is `MaterialError::Changed`.
- **Rotation without reload.** Every read still checks the configured file against its startup fingerprint, so a rotation without a reload refuses the read.

### Issuance: the next attempt gets the newest version

- **Issuance paths.** Preparation and accepted preparation issue the lease from `login_refresh::newest_source`, so the lease's `material_digest` is the newest version.
- **Lease reads.** These paths serve a lease's own bytes through `login_refresh::lease_source`:
  - initial grant
  - renewal private read
  - phase observations
  - material fetch
- **How refreshes spread.** Each node's refresh reaches every other node on its next attempt.
- **Version delivered with the material.** `POST /v1/worker/profiled/material` returns an `aq-material-version` response header (`material_api::VERSION_HEADER`) for subscription logins. It carries the decimal version of the delivered bytes, with the configured file as 0. The node echoes it as `MaterialRefresh.base_version`.
- **Runner channel header.** On these runner-permit routes, `aq-runner-channel` is required only when the bearer is a delegated permit (`aqp_…`). It is plain JSON of the `RunnerChannelIdentity`, sent once and at most 8 KiB, and must equal the permit binding. A node-credential request needs no channel header.

### Renewal: identity, not bytes

- **Rule.** `login_refresh::renewable(lineage, lease_digest)` runs inside the renewal and phase-continuation transactions, after the history rows are locked. It accepts the lease when both of these hold:
  - The lease digest names a version of this credential: the newest matching entry, or version 0 for `base`.
  - No `operator` entry is newer than that version.
- **What does not refuse renewal.** A refreshed revision of the same credential never refuses renewal, whichever lease wrote it. This replaces the old fence (`lease digest == startup fingerprint`), which refused every renewal after an attempt's own refresh.
- **What refuses renewal.** These refuse with the existing uniform conflict:
  - an operator replace
  - a rotated configured file plus reload, because the lease digest no longer names a version
- **Other kinds.** API-key credentials keep `base == lease digest`.

### Write-back endpoint (compare-and-swap)

- **Route and authentication.** `POST /v1/worker/profiled/material/refresh` uses the lease's delegated permit:
  - `authenticate_runner(..., FetchPreparationMaterial)`
  - `require_binding`
  - the same attempt, hot-state, registry, lease-admission and node-policy checks as the material fetch
- **Request format.** The body is the login file, at most 64 KiB. The `aq-material-refresh` header carries a base64url `MaterialRefresh` document with these fields:
  - `schema_version` 1
  - `operation_id`
  - `identity`
  - `binding`
  - `lease_id`
  - `base_version` ("N+1 based on N")
  - `content_digest`
- **Order of checks.** Under the credential's advisory lock, the endpoint checks in this order:
  1. **Replay.** A replay of (lease, digest) returns `{accepted: true, version: <original>, replayed: true}`.
  2. **Renewable.** A lease that is no longer renewable (operator replace, rotation) gets the uniform `409` with no body detail.
  3. **Version CAS.** If `base_version` is not the current version, the response is `409 {accepted: false, version: <current>, replayed: false}`. The server keeps its current version, because someone newer won.
  4. **Identical bytes.** Bytes equal to the current version return `{accepted: true, version: <current>, replayed: true}`.
  5. **Accept.** Otherwise the endpoint appends a `refresh` entry and returns `200 {accepted: true, version: N+1, replayed: false}`.
- **Responses never contain login bytes.**

### Operator import/replace

`aq-server import-login --vault <dir> --credential <id@version> --revision <n> --source <file>` stores a login file or token as the newest version of a configured, enabled subscription-login credential, and prints that version.

- **Source checks.** The source must be a regular file (`O_NOFOLLOW`) of 1..=65536 bytes. There is no format check, because a Claude token is plain text.
- **Operator always wins.** The upload is appended as an `operator` entry under the same advisory lock, even if its bytes equal the current version. This stops renewal of every lease issued before it. Later write-backs from those leases are refused, and the next attempt fetches the operator's version.

### Secret handling

- **Memory.** Login bytes are held in `Zeroizing` buffers.
- **Outputs.** Login bytes never appear in tracing fields, errors, `Debug` output, prompts or responses.
- **SQL.** SQL holds sha256 digests and sizes only.

## Verification

Run 2026-11-04 with `RUST_TEST_THREADS=2`, each command under `timeout 900`, against `scripts/test-db`.

| Command | Result |
| --- | --- |
| `cargo test -p aq-server --test subscription_refresh` | 7 passed: `writeback_accepts_newer_version`, `writeback_rejects_stale_base_and_returns_current`, `operator_replace_wins_over_agent_writeback`, `material_fetch_reports_login_version`, `writeback_is_idempotent_on_lease_and_digest`, `writeback_refuses_oversize_and_api_key`, `stored_revision_file_is_owner_only` |
| `cargo test -p aq-server --test native_initial_grant subscription` | 4 passed in about 8 s: `refreshed_revision_does_not_refuse_own_renewal`, `operator_rotation_refuses_renewal`, `configured_file_rotation_refuses_renewal`, `second_live_fixture_is_refused_not_deadlocked` |
| `cargo test -p aq-server --lib login_` | 3 passed: `only_operator_upload_stops_renewal`, `store_is_exclusive_owner_only_and_digest_checked`, `source_must_be_a_small_regular_file` |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy -p aq-server --all-targets -- -D warnings` | clean |

### The hung renewal test

An earlier `native_initial_grant subscription` run hung for more than 3 hours. The cause was not the simulated clock. `operator_rotation_refuses_renewal` then held both rotation cases, and it built its second fixture with `let mut c = login_setup().await`. Shadowing does not drop the first `c`, so that fixture still held the process-wide selection fixture gate (a non-re-entrant `tokio::sync::Mutex` in `tests/common/selection.rs`). The second setup waited on that gate forever.

- **Test fix.** The two cases are now separate tests, `operator_rotation_refuses_renewal` and `configured_file_rotation_refuses_renewal`.
- **Fixture fix.** The gate now records the thread holding it, as `FixtureGate`. Building a second fixture on the same test thread while the first is alive panics at once, with "the fixture gate is not re-entrant", instead of deadlocking.
- **Regression test.** `second_live_fixture_is_refused_not_deadlocked` checks the refusal and has a 60 s backstop timeout.

## Test notes

- **`writeback_accepts_newer_version`.** Version 1 is based on 0 and version 2 is based on 1. Each is accepted, and the newest source is version 2.
- **`writeback_rejects_stale_base_and_returns_current`.** A second "1 based on 0", and a base from the future, both get `409 {accepted: false, version: 1}`. No row is written, and the current version is kept.
- **`operator_replace_wins_over_agent_writeback`.** After a refresh to version 1, `import-login` stores version 2 as `operator`, and that version becomes the newest source.
  - A write-back from the earlier lease is refused, whether its base is 1 or 2.
  - A second identical operator upload still wins, as version 3.
  - An unknown credential is refused.
- **`refreshed_revision_does_not_refuse_own_renewal`.** The test grants, renews, writes back twice (based on 0, then 1), and then renews twice more.
  - This is the regression for the old fingerprint fence.
  - That fence refused only after a refreshed lease was re-issued, so the test is a forward guard rather than a red-first proof.
- **`operator_rotation_refuses_renewal`** and **`configured_file_rotation_refuses_renewal`.** In both, renewal and write-back return 409.
  - `operator_rotation_refuses_renewal`: `operator_replace` leaves the configured file unchanged. The old fingerprint fence would have **allowed** this renewal, so this test proves the identity rule.
  - `configured_file_rotation_refuses_renewal`: the configured file is rotated and the config reloaded.
- **`writeback_is_idempotent_on_lease_and_digest`.** Covers the (lease, digest) replay, and checks that the next issuance source is the newest version.
- **`writeback_refuses_oversize_and_api_key`.** Covers the 64 KiB cap and the subscription-login-only rule.

- **`material_fetch_reports_login_version`.** The material fetch returns `aq-material-version: 0` for the configured file. After a write-back, a lease issued from the refreshed bytes maps to version 1.

## Follow-ups

- `calm-delta-18.8.1`: Codex device-auth enrollment and capture service (deferred by the rescope).
- `calm-delta-18.8.2`: per-login concurrent-lease cap, `max_concurrent_leases`/`refresh_safe` (deferred by the rescope).
- `calm-delta-18.8.3`: update ADR 0005 to the versioned CAS design.
- CROSS-REPO (aq-worker), recorded as a task comment:
  - Send `base_version` from `aq-material-version`.
  - Parse `RefreshOutcome`.
  - Add the Codex `auth.json` write-back client.
- **Edge case (nit).** An operator re-upload of bytes identical to a lease's own version leaves that lease renewable, because the credential is unchanged. Only a different upload stops renewal.
