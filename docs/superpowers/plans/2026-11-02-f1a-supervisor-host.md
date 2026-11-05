# F1a Supervisor Host Implementation Plan

> Execute inline with superpowers:executing-plans; preserve the prior SQL-owner checkpoint.

**Goal:** Run and reconcile one centrally owned supervisor generation through the pinned shared guardian/tmux host.

**Architecture:** A trusted local supervisor host joins an authorized CentralPlan to HostPlan 1, records physical launch facts durably, and uses the existing SQL SessionOwner for generation and permit authority. PostgreSQL serializes operations per actor across host processes. Restart observes the recorded guardian without launch; failed observation stops and preserves, requiring the shared host's opaque stopped proof before successor admission. The host consumes exact upstream source, including the shared Codex mapper and scoped Responses transport.

**Tech Stack:** Rust, sqlx/PostgreSQL, Linux CLOCK_BOOTTIME, tmux; aq-session-host 0.1.6 at source archive SHA-256 e68ad4071935cec0bf7d67b5ab52e5e30364c12869e8e6b99b964b8fd69f3f1a.

**Spec:** Task steady-harbor-87.1 and the pinned upstream docs/implementation/F1a-host.md; existing AQ-17 owner contract.

## Global Constraints

- Exact vendored dependencies and committed lockfile; no sibling dependency or upstream contract changes.
- Central actor/conversation/session only; no worker assignment, offers, enrollment or Docker access.
- Permits at most 120 seconds on one machine boot; never extend the original deadline.
- Unknown stop blocks replacement; retain all generations and MailHistoryFence.
- Provider bytes remain in the host's retained credential reader/transport, never commands, prompts or environment.
- Mail dispatch/replies, supervisor tool credentials, production Compose and waits remain out of scope.
- Use resource-gated focused/related-area checks as prime requires; document unavailable checks honestly.
- Cross-family adversarial review and a pushed PR are required before passing close.

## Review Focus

1. Concurrent starts/restarts must return the same physical guardian or refuse.
2. A crash at any durable launch callback must never authorize launch replay.
3. Cancellation and permit expiry must stop the writer without deleting scratch.
4. Unknown stop and stale/foreign-boot evidence must never admit a successor.
5. Credential or configuration substitution must refuse without disclosing bytes.

## Task 1: Pinned host bridge and physical acceptance

Files: vendor/aq-session-host-0.1.6 and companion pinned packages; crates/aq-supervisor-host/{Cargo.toml,src/lib.rs,src/bridge.rs,src/journal.rs,tests/supervisor_session.rs}; migration 0046 and state-format registration.

Interfaces: consume SessionOwner, HostPlan, HostAdapter, LaunchAuthority and SessionHost. Produce Supervisor::start, reconcile, renew and stop, with exact central-to-host validation and a caller-provided PlanAuthority. Retain callbacks in SQL; only verified physical observations generate owner events.

- [ ] Add physical tests supervisor_session_starts_under_central_permit and supervisor_restart_resumes_without_duplicate_session. Assert central identity, one launch, live guardian, matching permit and retained startup; restart the controller and re-adopt the exact guardian without a second pane.
- [ ] Run through aq test and retain the initial missing-API failure.
- [ ] Implement pinned dependency normalization, exact bridge joins, SQL callback journal, serialized start/reconcile/renew/stop and stopped-proof conversion. Never reuse a launch ID for another physical attempt.
- [ ] Add cancellation, expiry, uncertain-stop, mismatched-plan and concurrent-start cases; run the related host/owner suites.
- [ ] Commit.

## Task 2: Executable, credential boundary and image

Files: crates/aq-supervisor-host/src/{main.rs,adapter.rs,credentials.rs}; Dockerfile.supervisor-host; tests/credentials.rs.

Interfaces: trusted owner-only local configuration supplies exact approved plans and CredentialConfig.secret_file placement. Codex mapping consumes only resolved model metadata; the shared scoped transport owns provider bytes and bounds requests. Shared helper runs the guardian; native startup consumes the staged file.

- [ ] Write refusal/redaction and startup-input tests first, run red, then implement the binary and adapter.
- [ ] Add the image with pinned helper/library, tmux and explicit native-runtime qualification; no Docker socket or worker executable.
- [ ] Test the executable and build the image where available. Record native/provider qualification limits distinctly from physical fixture tests.
- [ ] Commit.

## Task 3: Evidence and submission

- [ ] Run fmt, targeted strict clippy, both exact named commands and related SQL/model suites using aq test.
- [ ] Update docs/implementation/F1a.md with fresh commands, counts, environment, commits and limits.
- [ ] Fetch/merge origin/main, resolve conflicts and rerun affected checks.
- [ ] Publish the final branch with guarded aq git push; request the mandated cross-family review through the supervisor, retaining the claim while awaiting delivery.
- [ ] Resolve findings, refresh review if code changes, open the required PR and close truthfully with evidence.
