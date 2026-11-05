# I0 server findings (bright-nexus-44)

This change handles the five server findings reported from I0 runs 9 and 11.
Qualification uses synthetic credentials, in-process HTTP, disposable PostgreSQL
and pure relay fixtures. It does not repeat a live provider run or qualify MS1.

| Finding | Disposition and evidence |
| --- | --- |
| Control/output relay polls compete and produce 409 storms | Fixed. `Runtime` reserves one control permit and one output permit per node/attempt channel, preserving the delegated authentication scope merged from main. Both share the existing 32-channel bound. Duplicate requests within a lane still return 409. Output delivery continues while the control permit is held, without gaining control/input dispatch rights. `terminal_output_poll` and the runtime unit tests cover delivery, exclusion, capacity, attempt isolation and permit release. |
| Generic human 422 for an input deadline over 2000 ms | Fixed. The relay produces `InputDeadline` after authorization and exact-operation replay checks; the server preserves a safe `input_deadline` envelope through dashboard normalization. The message names the 1–2000 ms rule and the server-time header. The human request supplies `expires_at_ms`; the server assigns `issued_at_ms` when building `InputCommand`. Finite and epoch input retain the same rule. Tests cover both deadline boundaries, rejected queue preservation, exact replay and payload redaction. |
| Stopped, unsettled attempts retain `holds_task` | An unconditional release route is explicitly declined. Physical stop proves execution ended, but does not prove retained source or external effects were settled. Schema33/39 enforce an owner-verified release; removing the hold would let a replacement overlap unresolved prior effects. I0 produced no result, so success settlement cannot be invented. The existing qualified settlement/completion owner remains authoritative. `phase_terminal_stopped::paired_stop_does_not_qualify_old_sql_result_or_release_owners` now also attempts a direct hold clear and verifies the database refuses it without changing state. A recovery owner for unsuccessful attempts needs the B4/D3 settlement contract and worker recovery evidence; it is not supplied by this change. |
| Missing `[terminals]` causes late registration 503 | Fixed for enabled profiled execution. Config loading refuses enabled profiled project policies without terminal protection settings, before database access or listeners. Dormant policies and servers without profiled execution remain valid. Existing key loading validates configured files. The config regression tests missing settings, disabled policies and valid synthetic keys. |
| Missing registry returns 500; redacted refusals lack matching logs | Fixed missing-row handling in profile catalog inspection, intelligence selection and the admission fence: absent registry rows return explicit 409 conflicts. Database failures still fail closed. Human refusal logs now carry the returned request ID, public code/status and, when normalized, the original safe error class/status. Worker refusals log their safe code/status. Messages, request bodies, credentials and terminal bytes are excluded. Tests exercise missing registries and match response IDs to redacted log lines. |

The isolated output-poll 500 and terminal GET 503 have no retained diagnostic cause
in the task evidence. Changing those status semantics or treating failed reads as
success is explicitly declined: persistence, owner replacement and unavailable
keys must fail closed. Existing output-poll tests exercise stale boot, runtime
replacement and revoked credentials; the refusal logging test verifies that an
internal 500 normalized to human 503 has a matching diagnostic line. These tests
do not establish the cause of the historical one-off failures. The polling fix
addresses the reproducible concurrency fault, not every possible 500 or 503.

Merge compatibility: the delegated authentication helper added on main rejected
missing bearer headers as 403 before reaching the existing worker authenticator.
It now preserves the established `invalid_credential` 401 response for absent or
malformed bearer headers. Delegated permit scope checks remain authoritative.
The existing terminal HTTP boundary test covers this response.

## Affected MS1 contracts

The task keys refer to `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml`.
This change records implications; it does not edit those contracts.

| Contract | Implication |
| --- | --- |
| B6c terminal relay; B6a1 runner guardian/stop-and-preserve | Control and output polling can coexist. Registration still requires a current authenticated boot and installed runtime owner. Startup must provide terminal protection keys. |
| B6a2 runner route authentication; B2 permit/re-adoption | No authentication, boot, grant or historical-read fence is relaxed. Stale/replaced owners remain refusals. |
| B4 retry/replacement; B3 cancellation | Physical stop alone does not make a replacement eligible. Stop proof plus reconciled source/effects and verified release remain prerequisites. Stopped-without-result recovery remains unresolved by this patch. |
| D3 settlement/confirmed events | No result, graph completion, release receipt or completion event is fabricated for an unsettled attempt. The existing settlement owner retains the task hold. |
| C6a Codex runner configuration | Operator input still has a maximum 2000 ms intent lifetime; clients derive expiry from `aq-terminal-server-time-ms`. This change does not qualify a Codex image or use provider credentials. |
| MS1 steps 2–5 | Automatic next-start, failure retry and worker/server restart demonstrations must include real settlement/recovery evidence. Passing these synthetic server tests alone does not satisfy those steps. |

## Verification

Resource controls: `CARGO_TARGET_DIR=$PWD/target`, `CARGO_INCREMENTAL=0`,
`CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`. Database tests use the disposable
database reported by `scripts/test-db`.

- `cargo test -p aq-server --locked --offline --test config --test terminal_output_poll --test terminal_store --test intelligence --test profile_catalog --test project_bindings --test phase_terminal_stopped`: 68 passed, 0 failed, 1 existing selected-component test ignored.
- `cargo test -p aq-server --locked --offline --lib dashboard_api::errors::tests`: 3 passed.
- `cargo test -p aq-server --locked --offline --lib terminal::runtime::`: 2 passed after merging main, including delegated channel isolation and wakeup.
- `cargo test -p aq-terminal-relay --locked --offline --test fake_peers`: 25 passed.
- Direct `rustfmt --edition 2024 --check` on the changed Rust files and `git diff --check`: passed. `cargo fmt --all` could not load the excluded mapper vendor workspace on the original base; no manifest workaround was applied.

The final merge verification is recorded in the task comments and completion
evidence. The first merged run exposed the missing-header 403 corrected above
and one concurrent-controller `Internal` failure; the concurrent case passed
when rerun alone. The final `cargo test -p aq-server --locked --offline --test
terminal_store` rerun with `RUST_TEST_THREADS=1` passed all 21 tests, including
both previously failing cases. In total, the targeted coverage passed 98 tests,
with one existing selected-component test ignored.
