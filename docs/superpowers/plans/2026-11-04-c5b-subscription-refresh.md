# C5b Subscription-Login Refresh Write-Back Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a subscription-login credential safe to lease to a sandbox. This covers the server half of refresh write-back with refresh-lineage fencing, a per-credential concurrent-lease cap, the newest-revision issuance, a login import command, and the server side of Codex device-auth enrollment.

**Architecture:** A refresh lineage is an append-only chain of private material revisions. It is keyed by (credential identity, credential revision, base digest = startup fingerprint of the configured file). Secret bytes live in owner-only files under a configured `material_store` directory; only digests and metadata live in PostgreSQL. A single continuity rule replaces the "fingerprint == lease digest" checks in both renewal fences and the material fetch. The lease cap is a selection input (`leased_credentials`) mapped to `Unavailable`, and it is re-checked under an advisory lock at lease insert.

**Tech Stack:** Rust, axum, sqlx/PostgreSQL 17, aq-selection, aq-admission.

**Spec:** task calm-delta-18.8 (C5b) description and the owner decision of 2026-11-04 (`aq task show calm-delta-18.8`); the plan source is `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml` (C5b).

## Global Constraints

- Write-back is for `subscription-login` only, with a 64 KiB (65536-byte) cap. It is idempotent on (lease, content digest) and refused after revocation or rotation.
- `max_concurrent_leases` defaults to 1. A value above 1 requires `refresh_safe = true`. The count covers `pending` and `admitted` leases.
- A credential at its limit is `Unavailable` with a reason code (`credential_lease_limit_reached`), not `MissingCredentials`.
- No secret bytes in logs, prompts or inspection output. Files are owner-only (0600 in a 0700 euid-owned directory) and opened with O_NOFOLLOW. Fingerprints stay server-side.
- Defaults keep the legacy registry digest and the frozen selection vectors byte-identical (serde default plus skip_serializing_if).
- Named tests: `refresh_writeback_does_not_refuse_own_renewal`, `operator_rotation_refuses_renewal`, `second_lease_beyond_limit_is_unavailable`.
- The migration takes the next free number on origin/main (0076 at plan time) and sets `UPDATE meta SET value='76'`.
- Out of scope: runner install, write-back call and wipe (C6b); other harnesses' enrollment (I1–I3); notifications (F4); retry suppression (B4).

## Review Focus

- A refresh written by a *different* lease (a stale earlier attempt) after this lease's digest must refuse continuity unless `refresh_safe`. Task 3 test: `foreign_refresh_breaks_continuity`.
- Replaying the same (lease, bytes) must return the original sequence and not append. The same lease with different bytes appends. Task 3 test: `writeback_is_idempotent_on_lease_and_digest`.
- An oversized body (65537 bytes) or an api-key credential is refused with no row written. Task 3 test: `writeback_refuses_oversize_and_api_key`.
- A lease whose attempt has closed (lease `revoked`) cannot write back. This is covered by the lease-state fence in Task 3's endpoint test.
- An enrollment capture token is one-shot: a second capture is refused. Task 5 test: `enrollment_capture_is_one_shot`.

---

### Task 1: Credential lease policy and material store config

**Files:**
- Modify: `crates/aq-server/src/intelligence/config.rs` (CredentialConfig, IntelligenceConfig, validate)
- Modify: `crates/aq-server/src/intelligence/registry.rs` (digest placement variant)
- Modify: every `CredentialConfig { .. }` literal (src and tests) to add `..`-free fields.

**Interfaces:**
- Produces: `CredentialConfig.max_concurrent_leases: u32` (serde default 1), `CredentialConfig.refresh_safe: bool` (default false), and `IntelligenceConfig.material_store: Option<PathBuf>` (absolute; default None). Also `CredentialConfig::lease_limit(&self) -> Option<u32>`, which is `Some(max)` for SubscriptionLogin and `None` (uncapped) for ApiKey.

- [ ] Test `lease_limit_above_one_requires_refresh_safe`: validate refuses `max_concurrent_leases = 2, refresh_safe = false`, accepts it with `refresh_safe = true`, and refuses `0`.
- [ ] Implement. The registry digest adds a `Limited(cred, nodes, kind, max, refresh_safe)` placement only when the values are non-default, so the legacy and Kinded digests stay unchanged.
- [ ] Run `cargo test -p aq-server --lib lease_limit_above_one_requires_refresh_safe`; expect PASS. Commit.

### Task 2: Lease cap at selection and at insert

**Files:**
- Modify: `crates/aq-selection/src/domain.rs` (`SelectionInputs.leased_credentials: Vec<ArtifactRef>`, serde default and skip if empty)
- Modify: `crates/aq-selection/src/candidate_core.rs` (`CoreInputs.leased: &[ArtifactRef]`; after the delivery check, a profile in `leased` → `(Unavailable, "credential_lease_limit_reached")`)
- Modify: `crates/aq-selection/src/select.rs`, `central.rs` (pass `&input.leased_credentials`, or `&[]` for central), and the aq-selection test builders.
- Create: `crates/aq-server/src/profiled_execution/refresh/lease_limit.rs`
- Modify: `crates/aq-server/src/intelligence/resolver.rs` (populate), `preparation.rs:436` and `accepted_preparation/accept.rs:224` (insert guard)
- Test: `crates/aq-server/tests/subscription_lease_limit.rs`

**Interfaces:**
- Produces: `lease_limit::at_limit(pool_or_tx, registry, node_credentials) -> Result<Vec<ArtifactRef>>` and `lease_limit::admit_locked(tx, registry, &MaterialLease) -> Result<(), AppError>`. The latter takes `pg_advisory_xact_lock(hashtext('aq.lease-limit:'||identity))` and refuses when the live count (state IN pending, admitted, by credential_identity) is ≥ the limit.

- [ ] Test `second_lease_beyond_limit_is_unavailable` (selection fixture): mark credential 0 as subscription-login, re-register, and resolve once; it is Eligible. Insert a `pending` lease row for that credential identity, then resolve again. Assert the trace for that credential has `state == Unavailable`, its `reason_codes` contain `credential_lease_limit_reached`, and the state is not MissingCredentials. Revoke the row and resolve again; it is Eligible.
- [ ] Run it; expect FAIL (still Eligible).
- [ ] Implement the selector field, the resolver population and the insert guard.
- [ ] Run the test plus `cargo test -p aq-selection` (frozen vectors unchanged); expect PASS. Commit.

### Task 3: Refresh lineage store and write-back endpoint

**Files:**
- Create: `crates/aq-server/migrations/0076_subscription_refresh_lineage.sql`
- Create: `crates/aq-server/src/profiled_execution/refresh.rs` (module root: lineage queries and the continuity rule)
- Create: `crates/aq-server/src/profiled_execution/refresh/store.rs` (owner-only file write and read)
- Create: `crates/aq-server/src/profiled_execution/refresh/api.rs` (route)
- Modify: `crates/aq-server/src/profiled_execution.rs`, `src/lib.rs` (merge routes), `src/admission.rs` (classify `/v1/worker/profiled/material/refresh` as Bulk)

**Schema:** `credential_material_revisions(revision_id uuid pk, credential_identity, credential_revision, base_digest, sequence, origin in ('refresh','enrolled'), lease_id uuid null references admission_material_leases, content_digest, content_bytes 1..=65536, created_at_ms, unique(identity, revision, base, sequence), unique(identity, revision, base, content_digest))`, plus a partial unique index on (lease_id, content_digest), and the check `(origin = 'refresh') = (lease_id IS NOT NULL)`.

**Interfaces:**
- Produces:
  - `refresh::Lineage { base: String, entries: Vec<Entry> }` and `refresh::Entry { sequence: i64, origin: Origin, lease_id: Option<Uuid>, content_digest: String, content_bytes: i64, revision_id: Uuid }`.
  - `refresh::lineage_locked(tx, identity, revision, base) -> Result<Lineage>`.
  - `refresh::continuous(lineage, lease_id, lease_digest, refresh_safe) -> bool`.
  - `refresh::head(lineage) -> Option<&Entry>`.
  - `store::write(dir, revision_id, bytes) -> io::Result<()>` and `store::read(dir, revision_id, digest, bytes) -> Result<ProtectedMaterial, MaterialError>`.
- Request: `POST /v1/worker/profiled/material/refresh`. The body is application/octet-stream, the new login bytes (1..=65536). The header `aq-material-refresh` carries base64url JSON `MaterialRefresh { schema_version: 1, operation_id, identity: WorkerIdentity, binding: LaunchBinding, lease_id, content_digest }`. The response is JSON `{ "sequence": n, "replayed": bool }`. Refusals use one uniform Conflict.
- Auth: `authenticate_runner(FetchPreparationMaterial)`, `require_binding`, the node/boot/admitted-attempt fence, lease state `admitted`, hot not revoked, and status Preparing or Running. It also requires the credential to be enabled, subscription-login, its fingerprint to equal the lineage base, and `continuous(...)`.

- [ ] Tests (`tests/subscription_refresh.rs`, modelled on `tests/profiled_material.rs` setup with `kind: SubscriptionLogin` and a `material_store` directory): `writeback_is_idempotent_on_lease_and_digest`, `writeback_refuses_oversize_and_api_key`, `foreign_refresh_breaks_continuity` (unit test of `continuous`), and `stored_revision_file_is_owner_only` (mode 0600, the bytes never appear in the response).
- [ ] Run them; expect FAIL (404 route).
- [ ] Implement the migration, store, rule and route. The file is written before the row insert; a crash leaves an orphan file and never a row without a file.
- [ ] Run the tests; expect PASS. Commit.

### Task 4: Renewal continuity and newest-revision issuance

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/renewal/current.rs` and `phase_execution/current.rs` (replace the fingerprint equality with `refresh::continuous_locked(tx, loaded, index, &lease)`)
- Modify: `crates/aq-server/src/profiled_execution/material.rs` and `material/issuance.rs`; `preparation.rs` and `accepted_preparation/accept.rs` (look up the head before the blocking read and pass `Option<HeadSource>`)
- Modify: `crates/aq-server/src/profiled_execution/material_api.rs` (serve the lineage entry whose digest equals the lease digest)
- Test: `crates/aq-server/tests/native_initial_grant/published/renewal.rs`

**Interfaces:**
- Consumes: Task 3 `refresh::*`.
- Produces:
  - `refresh::continuous_locked(tx, loaded: &LoadedRegistry, index: usize, lease: &MaterialLease) -> Result<bool, AppError>`. For an ApiKey credential it is exactly `fingerprint == lease digest`.
  - `refresh::HeadSource { revision_id, digest, bytes, dir }`.
  - `material::issue_for_selection_from(.., head: Option<HeadSource>)`, with `issue_for_selection` delegating with None.

- [ ] Test `refresh_writeback_does_not_refuse_own_renewal`: a subscription-login variant of the published renewal setup. Grant, write back new bytes through the endpoint with the attempt's own lease, then renew; expect 200 Renewed.
- [ ] Test `operator_rotation_refuses_renewal`: same setup. (a) Capture an `enrolled` revision through Task 5's capture; renewal is refused. (b) In a fresh case, write a new configured file, reload the registry and re-register; renewal is refused.
- [ ] Run them. Expected: t1 passes against the old fence (the lease digest still equals the fingerprint) and guards the replacement. t2(a) FAILS: renewal is still accepted after an enrolled revision. t2(b) passes, since the fingerprint changed. Record the output.
- [ ] Implement the continuity calls, the head-based issuance and the fetch.
- [ ] Run both tests plus `tests/profiled_material.rs` and the existing `native_initial_grant` renewal tests; expect PASS. Commit.

### Task 5: Codex device-auth enrollment (server side)

**Files:**
- Create: `crates/aq-server/src/intelligence/enrollment.rs`; the migration from Task 3 adds `credential_enrollments`.
- Modify: `crates/aq-server/src/intelligence.rs` (admin routes)

**Interfaces:**
- `POST /api/intelligence/enrollments` (RequireOwner) takes `{ credential_profile: ArtifactRef, credential_revision: u64 }` and returns `{ enrollment_id, capture_token, expires_at_ms, sandbox }`.
  - The `capture_token` is 32 random bytes, base64url, shown once; only its sha256 is stored.
  - The TTL is 15 min.
  - `sandbox` is a static description: a private `CODEX_HOME`, `config.toml` containing `cli_auth_credentials_store = "file"`, the command `codex login --device-auth` (the device code appears in the owner's terminal), then a capture of `$CODEX_HOME/auth.json`.
- `POST /api/intelligence/enrollments/{id}/capture` (header `authorization: Bearer <capture_token>`, octet-stream body 1..=65536) appends an `enrolled` lineage entry and marks the session captured (one-shot).

- [ ] Tests: `enrollment_capture_is_one_shot` (a second capture is refused; a wrong token is refused; an api-key credential cannot be enrolled). `operator_rotation_refuses_renewal` (a) uses this capture.
- [ ] Implement; run; commit.

### Task 6: Login import command

**Files:**
- Create: `crates/aq-server/src/intelligence/login_import.rs`
- Modify: `crates/aq-server/src/main.rs` (`ImportLogin { --vault, --source, --name, --replace }`)

**Interfaces:**
- `login_import::import(source: &Path, vault: &Path, name: &str, replace: bool) -> anyhow::Result<PathBuf>`:
  - Reads the source with O_NOFOLLOW; the file must be regular, 1..=65536 bytes and a JSON object.
  - Writes `<vault>/credentials/<name>.json` with 0600, using O_EXCL unless `replace`, via a temp file and rename.
  - Returns the path. The CLI prints a TOML stanza with `kind = "subscription-login"` and the `secret_file` path, never bytes.

- [ ] Test `import_copies_login_owner_only_without_printing_secret`: the destination is mode 0600 with identical bytes; a non-JSON source is refused; overwriting without `replace` is refused; the stanza string contains no source bytes.
- [ ] Implement; run; commit.

### Task 7: Evidence and follow-ups

- [ ] Write `docs/implementation/C5b.md`: the store design, the continuity rule, the Claude Code setup-token evaluation (from the 2026-11-04 operator spike), the commands and results for t1–t3, and the follow-ups.
- [ ] Follow-ups:
  - CROSS-REPO FOLLOW-UP (aq-worker): publish the `MaterialRefresh` schema and fixtures and a `RunnerOperation::WriteBackMaterial`.
  - A same-repo task to bind write-back to the C5a2 permit.
  - A docs task for the architecture doc (credential lineage).
