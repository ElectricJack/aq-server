# F1b follow-up: supervisor binary reply access

Task: `steady-harbor-87.2.2`. Prerequisite: F1b on
`aq/steady-harbor-87.2`, checkpoint
`b807f893fa50089c1b7108168a3a4a4faa5973aa`, merged with supervisor authorization.
The task branch retains that prerequisite's history.
The final merge of `origin/main` advanced the base to `d616375f`, including
the reservation-consumption follow-up and migration 49. Focused tests, exact
acceptance and strict clippy passed again against this merged base before publication.

`main.rs:99-105` now calls `Supervisor::enable_replies` after startup returns
Running. `conversation.rs` performs the verified handoff, requires a handed-off
Running generation, and issues through the existing server credential owner.
It bounds the credential lifetime by the original generation deadline and uses
the existing 0600 create-new launch-directory installer. The actor fence covers
credential lookup/issuance/installation. On controller restart, it authenticates
and reuses the installed token instead of revoking the live session's credential.
An unknown handoff, inactive generation or invalid retained token refuses access.

The root native wrapper waits for installation before spawning Codex. It reads
the protected launch credential and writes a 0600 copy into the private native
home, transferring ownership of that copy to native UID 10001. The provider key
stays in its existing root-only placement and scoped transport. The supervisor
recipe explicitly exports `AQ_SUPERVISOR_CREDENTIAL_FILE` and
`AQ_SUPERVISOR_DISPATCH_ID` to both Codex and its tool shells, and enables tool
networking for AQ replies. Arguments and environment contain only the path and
dispatch ID, never token bytes. No worker crate, launch recipe, mount or payload
is changed; `/host` and `/native-homes` remain supervisor-only volumes (W1/SBX17).
The AQ machine listener origin comes from deployment/startup context.

## Verification

Run through the foreground AQ resource gate:

```
F1B_WIRING_CHECK=<focused|acceptance|clippy|format> \
  DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
  aq test docs/implementation/F1b-wiring-evidence/test_cargo.py -q --timeout=1800
```

The wrapper fixes Cargo jobs to 2, Rust test threads to 2, disables incremental
builds/debug artifacts, and records each exact Cargo command's full output.

- `focused`: host library/binary, adapter and credential tests. The new waiting
  test uses paused Tokio time, including the create-before-write interval; it
  adds no real sleep. Staging tests cover mode 0600, no overwrite, unsafe source
  rejection and already-expired installation.
- `acceptance`: exactly
  `owner_message_reaches_supervisor_and_reply_is_recorded`. The existing real
  PostgreSQL/tmux fixture now invokes the binary's activation method, first
  refusing a mismatched handoff without issuing any token, then reconciling the
  exact input. It checks restart reuse, generation-bounded TTL, mode 0600, a
  reply authenticated using the native-home copy, and refusal after stop.
- `clippy`: strict `aq-supervisor-host --all-targets`, with `-D warnings`.
- `format`: `cargo fmt --package aq-supervisor-host --check`.

Recorded results: `focused.txt` has 7 passed / 0 failed across the library,
binary, adapter and credential targets. `acceptance.txt` has exactly 1 passed /
0 failed / 11 filtered out. `format.txt` is empty (successful format check), and
`clippy.txt` ends with a successful strict check with no warnings.

The initial compile failure (`focused-initial.txt`) identified that server
`AppError` does not implement `std::error::Error`; the wiring now maps issuance
and authentication refusals to bounded host errors.

## Scope

No image rebuild, real Codex/provider qualification or full-suite run. Existing
F1b follow-ups own additional command scopes, explicit revocation, reply cursoring
and successor turns. The test fixture verifies the production activation and
staging helpers, not a root container running a live Codex session. Keep the
supervisor's private volumes absent from all worker mounts.
