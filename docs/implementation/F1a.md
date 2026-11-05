# F1a: central supervisor host and durable lifecycle

Task `steady-harbor-87.1`. The earlier prerequisite block is resolved by
`crisp-nexus-12`. This increment implements the physical bridge, executable and
image on top of the previously committed SQL session owner. Independent
validation is recorded below. The upstream request-publication race is fixed
by the 0.1.7 pin requested in the latest reopening. Review revision 2 addresses
the operator-scoped graceful-stop and crash-documentation nits below. The operator
has approved review revision 2; the final disposition is recorded below.

## Revision 2: graceful stop and crash recovery notes

Task `steady-harbor-87.8` preserves the reviewed `5f945efc` history and merges
it into the assigned revision branch. Per supervisor publication instructions,
this follow-up depends on aq-server PR #26; it does not push the original F1a
branch or merge either PR.

`run` registers SIGTERM and SIGINT handlers before launch. Either signal enters
the existing `Supervisor::stop` path, including the guardian's stopped-writer
proof, so the default `docker stop` signal reaches graceful shutdown. Operators
must configure a stop grace period greater than the 10-second `wait_stopped`
budget plus database lock/statement timeouts and scheduling overhead; Docker's
default 10 seconds is insufficient. Container/guardian loss can still require
inspection and never supplies stopped proof by itself.

A crash after `StartCommitted` but before the host start-intent fact requires
operator inspection of `StopUncertain`; a crash after `Prepared` but before the
start commit is reconciled to `CancelledBeforeStart` and requires a successor
plan rather than retrying the same plan.

Review dispositions: `cmt-1848a5684723` is addressed by the signal handlers and
targeted OS-signal test; `cmt-116224e48c17` is addressed by the crash-window
sentence here and in the host README. Per the explicit operator override,
bounded retry (`cmt-b61d99d685f5`), typed conflict (`cmt-0173af107bb4`) and
native `setsid` (`cmt-825cd2f83da1`) are deferred to a separate operator-filed
follow-up and are not implemented here. `cmt-9c37fe387775` agrees with the
existing 0.1.7 evidence; that historical evidence is retained unchanged.

Targeted validation: `F1A_CHECK=shutdown_signals aq test
docs/implementation/F1a-evidence/test_cargo.py -q --timeout=1800` passed its one
AQ wrapper test and executed `cargo test --locked -p aq-supervisor-host --bin
aq-supervisor-host shutdown_receives_sigterm_and_sigint`: 1 passed, 0 failed,
0 filtered out. The real SIGTERM/SIGINT test took 0.00 seconds after 5m24s of
fresh compilation; no sleeps or physical lifecycle rerun. Cargo output is
retained in `F1a-evidence/shutdown_signals.txt`. `rustfmt --check --edition 2024
crates/aq-supervisor-host/src/main.rs` and `git diff --check` also passed.
`aq git pull --branch main` followed by final `git merge origin/main` confirmed
main `71e08197` is already included. This revision does not rebuild the image
or claim a full-suite or new provider/container qualification.

## Source and integration

The dependency is the published aq-worker commit
`4992a16d6c2f794d692803fbe25e69ea400a97db` (aq-worker main, PR #22):
aq-session-host 0.1.7, shared
Codex mapper 0.1.1 and scoped Responses transport 0.1.0. The exact source archive
SHA-256 is `ab37cb192870f07290b4aff887b57b73783931ba5dfc248184da483c99036bdb`.
The archive and its receipt are retained in `vendor/`. Runtime sources are
unchanged; Cargo manifests normalize workspace inheritance and dependency paths,
with originals retained as `Cargo.toml.upstream`. The existing session and
terminal-relay protocol sources match the published closure. Docker verifies
the archive digest and compares the three consumed runtime source trees.
There is no sibling-checkout dependency. All 1,201 receipt file digests and
the five consumed runtime/protocol source trees were verified against the
release archive (`F1a-evidence/pin-0.1.7.txt`). Mapper and transport sources are
unchanged from the prior pin.

Reopened checkout base: `b183e0cdbcdd7c5e597e3c0ea1b0465c19cfcc7e`.
Plan: `docs/superpowers/plans/2026-11-02-f1a-supervisor-host.md` (128f7f50).
Implementation checkpoint: 0c014048; async stack fix and migration regression:
0043e894. Integration checkpoint: ef114a62, merging
`origin/main` at ed027c42. Main already owns migration 45 for central delivery;
this branch's unintegrated owner and host migrations are now **46 and 47**.
The server's migration list, readable formats and image label advance to 47.
The old branch-only format-45 checkpoint was never a production upgrade target.

Claim 4 resumed at `5159be86ee30981d57b39b1fa278684b27814759`, which already
merges main `3919a844b98021f07a00f1884ef2ed06b4278abc`. Guarded
`aq git pull --branch main` confirmed that base. Existing published checkpoints
and merge ancestry are retained. This increment changes only the runtime pin,
its Cargo/Docker/fixture references, and evidence. The pin implementation is
`317815c6a804fd076e9207da0cef784b4491796a`. Final `git merge origin/main`
integrated `ae0c89b8f7cb08a30823a5b4fb57cce02da9f291` as
`ec2ba0fd90a30e7e5c4eb4b268b55ae9459146f1`; that merge brings unrelated
scheduling-freshness changes. The two supervisor acceptance tests are checked
again on this merged source.

## Implemented behavior

- `aq-central-authority::SessionOwner` serializes the central actor head and
  retains immutable generations. It owns permits (at most 120 seconds), the next
  sequence, strictly later renewal expiry, original deadline, boot identity and
  terminal-state rules. Every successor derives MailHistoryFence from all retained
  generations. Refused late renewals still preserve the stopped/expired state.
- `aq-supervisor-host::Supervisor` joins CentralPlan to the exact HostPlan, reads
  actual CLOCK_BOOTTIME and boot ID, and holds an actor advisory lock on a dedicated
  PostgreSQL connection throughout physical operations. Immutable SQL callbacks
  retain intent, guardian, start intent, observations and stopped proof. A reserved
  launch is never physically replayed after a crash or failure.
- Restart checks current admission and re-adopts only the same recorded guardian
  with a live same-boot permit. A centrally committed but unapplied renewal is
  delivered only to the still-live old permit. Cancellation, expiry and failed
  observation stop and preserve scratch. Only the shared guardian's opaque
  stopped-writer proof becomes a verified central Stopped event. Guardian loss
  records StopUncertain and prohibits replacement; disappearance is not proof.
- Authorization is checked at plan/permit admission, again immediately before the
  durable physical start intent, and on re-adoption. Launch or late Running-report
  errors attempt stop/recovery without erasing the reservation or scratch.
- `aq-supervisor-host run|stop|status CONFIG` is a trusted local installation API,
  not a remote command endpoint. CONFIG admits one exact plan and resolved model;
  changing/removing it revokes that process's admission. The reusable PlanAuthority
  interface lets the central application supply current policy and original-mail
  authorization in F1b. No permissive production verifier is installed.
- The shared Codex mapper supplies the closed 0.166.0 recipe. A guardian-owned
  wrapper retains the scoped Responses transport; controller restart therefore
  does not rotate the still-live transport. The provider key is read from the
  protected `secret_file` placement only in that root wrapper. Native code runs
  as UID/GID 10001 with no supplementary groups, no_new_privs, a clean environment
  and startup via stdin. It receives only a local, bounded transport capability.
  The native executable is checked by SHA-256 and executed through its retained
  descriptor. Provider bytes never enter argv, environment, prompts or the native
  sandbox. Credential identity/content changes revoke the transport.
- `Dockerfile.supervisor-host` builds only the host and shared helper as runtime
  programs, plus tmux and TLS support. It has no worker enrollment/offers, Docker
  socket integration or production Compose service. Installation requirements and
  exact config fields are documented in `crates/aq-supervisor-host/README.md`.

## Current 0.1.7 verification

The operator explicitly limited this reopening to the named startup and restart
filters. They run through the same resource-gated adapter described below,
using real tmux and disposable PostgreSQL on Linux WSL2, Rust/Cargo 1.96.1.

At pin commit `317815c6`, both exact required workspace commands passed their
one intended physical test (1 passed, 0 failed, 10 host cases filtered out):

- `cargo test --locked --workspace supervisor_session_starts_under_central_permit`
  — wrapper 838.41 seconds, including 13m37s fresh compilation.
- `cargo test --locked --workspace supervisor_restart_resumes_without_duplicate_session`
  — wrapper 9.15 seconds.

Logs: `F1a-evidence/named_start.txt` and `named_restart.txt`. Each command uses
`F1A_CHECK=named_start|named_restart DATABASE_URL=<disposable-test-db> aq test
docs/implementation/F1a-evidence/test_cargo.py -q --timeout=1800`. The restart
command used the compiled pre-merge artifacts; the post-merge runs below provide
the final-source evidence.

On merged source `ec2ba0fd`, both focused commands also passed their one named
physical test (1 passed, 0 failed, 10 filtered out):

- `cargo test --locked -p aq-supervisor-host --test supervisor_session supervisor_session_starts_under_central_permit`
  — wrapper 240.98 seconds, including 3m56s package compilation.
- `cargo test --locked -p aq-supervisor-host --test supervisor_session supervisor_restart_resumes_without_duplicate_session`
  — wrapper 7.34 seconds.

These use `F1A_CHECK=merged_start|merged_restart` with the same AQ invocation;
logs are `F1a-evidence/merged_start.txt` and `merged_restart.txt`. These are
actual real-guardian/tmux passes, not zero-match filters. No broad suite or
container rebuild was run for this pin update. The prior image digest below
is historical, not a 0.1.7 build.

## Prior 0.1.6 verification

Linux WSL2, Rust/Cargo 1.96.1, real tmux, disposable PostgreSQL from
`scripts/test-db`. Each SQL test creates its own database. Every test/build/lint
uses `aq test` through `F1a-evidence/test_cargo.py`, with Cargo build jobs 2,
debug info/incremental compilation disabled and bounded test threads. The image
also builds with two Cargo jobs and an image-specific BuildKit target cache.

Invocation: `F1A_CHECK=<check> DATABASE_URL=<disposable-test-db> aq test
 docs/implementation/F1a-evidence/test_cargo.py -q --timeout=1800`.
The adapter records exact commands and separate logs in `F1a-evidence/`.

Retained red evidence:

- `host_red.txt`: missing physical API before implementation.
- `credentials_red.txt` and `adapter_red.txt`: missing credential/adapter modules.
- `revocation_red.txt`: live restart incorrectly survived policy revocation.
- `start_revocation_red.txt`: revocation during adapter preparation incorrectly
  allowed physical launch. The new pre-start check and error cleanup address it.

The prior 0.1.6 post-merge host suite passed the credential and mapper checks and
10 of 11 integration cases (ten physical lifecycle cases plus one database
upgrade case). The one failure was the named startup case hitting the published
runtime race below. The same startup case passed earlier; the failure is retained
rather than hidden by an automatic retry. Nested SQL-owner futures are boxed at
composition boundaries after concurrency tests exposed debug-build stack growth.
The real format-45-to-47 upgrade preserves an existing central delivery intent.
Both exact named workspace commands passed, each executing its one intended
physical integration test (the other ten host cases filtered out). The focused
SQL session-owner suite passed 8/8 and authority-model suite passed 16/16. Host
and related authority/server clippy checks passed with warnings denied. The
state-format registration check passed 1/1 and `cargo fmt --check` passed.
Corresponding logs are `owner.txt`, `model.txt`, `host_clippy.txt`, `clippy.txt`,
`format_version.txt` and `format.txt` (successful formatting emits no output).

The exact task acceptance commands are retained in the adapter as `named_start`
and `named_restart`; they run real guardian/tmux tests, not persistence-only
fixtures. Full command output is retained in `named_start.txt` and
`named_restart.txt`:

- `cargo test --locked --workspace supervisor_session_starts_under_central_permit`
- `cargo test --locked --workspace supervisor_restart_resumes_without_duplicate_session`

The image check builds the actual image and runs a fixture native CLI inside it.
The CLI must receive the exact startup stdin, run as UID 10001, have no provider
key in its environment and fail to read the root-owned credential file. It uses
a dummy key, an isolated network and no provider request. This checks process
separation, not native/provider qualification. The image check at source
checkpoint 0043e894 passed with image digest
`sha256:b102f5ed4910946cd7f2b22fa6d492fa9de8589c513a115b102208899031ad54`;
the build and boundary result are retained in `F1a-evidence/image.txt`. The
subsequent lint cleanup only avoids allocating a PathBuf for a path comparison.

## Resolved upstream publication race

`F1a-evidence/upstream_start_publication_race.txt` records a natural failure:
`session ended before startup: startup failed: EOF while parsing a value at line
1 column 0`. In the pinned 0.1.6 source, `SessionHandle::request` (lib.rs:907-924)
calls `durable_new`, which opens the final request path with create_new before
writing JSON (lib.rs:1106-1114). The guardian sees `start.request` existence and
immediately parses it (supervisor.rs:208-211); a reader scheduled before the write
sees empty/truncated JSON and enters Stopping (supervisor.rs:306-310). State
replacement uses atomic rename, but this first request publication does not.
The failure is safe refusal, not execution without authority, and the central
bridge attempts verified cleanup. It prevented reliable startup acceptance
with the 0.1.6 pin.

The upstream owner resolved **bold-stone-39** in aq-worker main merge
`4992a16d6c2f794d692803fbe25e69ea400a97db`, publishing 0.1.7. The new
`durable_write_with` writes and syncs a private temporary file before
`renameat2(RENAME_NOREPLACE)` publishes a new request. Existing-file exclusion
is retained. Three deterministic upstream tests cover partial-write visibility,
failed writes/existing entries, and competing publishers. The vendored source
is identical to that release; no consumer patch or replay retry was added.

The earlier failure remains in `upstream_start_publication_race.txt` and the
0.1.6 full-host log. It was reported as `msg-da16d0b744f349cbb654d80b47f3fd71`
and confirmed by `msg-50f798ec4a3c4d92adcd4a5b0627d2c8`. The operator's latest
reopen feedback authorizes this exact pin and targeted requalification.

## Limits and review

ResourcePolicy::CallerAdmission records admission, not hard cgroup enforcement.
The native executable is an explicitly mounted, digest-pinned operator-supplied
runtime; no real Codex/provider qualification or aggregate spend enforcement is
claimed. Private PID namespace and root/native UID separation are required; this
is not hostile-kernel isolation. A container/guardian loss can remain uncertain
and require operator inspection; it never authorizes replacement by inference.
Original-mail delivery/reply/tool credentials (F1b), production orchestration
(E1c), enqueue (F2), wait activation (H4/H6) and final system security review (J4)
remain outside scope. No architecture/specification files were changed.

The full unfiltered workspace suite is not claimed: prime restricts this worker
to focused and related-area checks. The two required filtered workspace commands
are explicit acceptance obligations. Cross-family adversarial review and the
required PR/review disposition are recorded in the task comments and review
request.

## Final closure verification

`aq review show --review-id rev-nimble-horizon --json` now reports revision 2
`approved`, decided by `human:local-operator` at `1793691245.3243306`, and gate
`gate-5978d3673254` resolved with resolution `approved`. The original review
decision is `ask-decision-30a79a85ec1841a28a97f0df28691e79`; reviewer
`wise-summit-84` found no blocking defects in implementation `5f945efc` (PR #26).
The revision-2 decision approves graceful-stop and crash-window fixes and assigns
remaining nits to `quick-flare-47`.

Final `git merge origin/main` integrated main
`696d1e9dd480fa2d1eca1421a306f367641fae30` as
`26a7fdaaa7fe86f873bec6a16fbf4ddbc529587a`. The central-authority and supervisor-host
trees and retained evidence match approved revision-2 source
`6f34e72f5e8c2772963837febacbd585f5895736` exactly. No new implementation changes
or repeated tests were needed. Existing startup, restart and signal-test logs
each record their intended test passing. This closure update changes only this
evidence document. The worker did not merge PR #26.
