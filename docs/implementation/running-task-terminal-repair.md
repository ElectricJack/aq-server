# Running task terminal attachment repair

Task: a3c72f06-d1a6-428e-bf67-b0f1c734a8c9.

## Diagnosis and reproduction

The dashboard is consumed here as the pinned v0.1.5 release archive, source
revision `6775a0d46ffe5b80cbb2ca95fa54aae0580180c9`. The exact upstream
source was inspected from its public GitHub source archive in `/tmp`; no
external repository was edited. The repair is reviewable JavaScript in
`vendor/dashboard-terminal-fix`, applied to that verified release during the
server image build. The original release archive and pin are unchanged.

Trace through the supplied implementation:

1. The Agents session pane passes `session.terminal.id` to InteractiveTerminal.
   The server derives that reference from `terminal_sessions` for the attempt,
   matching the full session identity and current runtime in
   `dashboard_api/session.rs`. A terminal ID is distinct from an attempt ID.
2. `nativeTerminalClient.liveAttempt` reads `/api/v1/terminals/{terminal}` and
   resolves `terminal.session.attempt_id`. Previously it also called
   `/api/attempt-terminals/{attempt}/terminal/connect` with a throwaway client ID.
   It caught **all** errors and returned null.
3. A null result silently selected `connectInteractiveTerminal`, even for a
   docker-local attempt whose finite relay policy has input disabled. A refused
   or transiently failing discovery-time connect therefore bypassed the live
   attempt attachment entirely. No live screen replay or supported task keyboard
   path could follow. Successful discovery also acquired control twice with
   different client IDs, unnecessarily fencing an existing viewer.
4. The finite control adapter used for discovery additionally accepted only
   `finite_v1`. That is unnecessary for resolving an output-only attempt's
   identity; an output-only `epoch_v1` registration hit the same fallback.
   The supplied worker archive registers finite_v1, so epoch rejection is a
   separate compatible-server case, not evidence of the owner's active mode.
5. A browser socket could stay open indefinitely without a worker ready frame.
   Compact panes hid their status while connecting, making this an unexplained
   black pane. Pending writes also lacked an attachment identity check when
   executed, allowing an old socket's queued output to render after replacement.

`node --test scripts/test-dashboard-terminal.mjs` first executes the **original
pinned discovery function** with a refused preflight connect and asserts that it
selects the input-disabled finite relay and creates no live socket. The remaining
cases execute the corresponding functions extracted from the **actual patched
bundle**, exercising task terminal selection through its native client. This
establishes a concrete failure path in the shipped artifact; it does not prove
which transport response the owner's deployment returned.

## Resulting behavior and authority

Discovery now only reads and validates terminal identity, session revision and
policy. The selected live client acquires control once, using the same client ID
and returned generation for its attempt socket. Failed discovery or attachment
shows an unavailable/reconnect state, without silently switching transports.
Finite input terminals keep their existing receipt-based relay. Unsupported
keyboard modes fail visibly instead of sending finite mutations under another
protocol. Output-only remote attempts may resolve to their live socket without
requiring finite keyboard support.

Ready/reset and initial tmux paint remain ordered before live output. Queued
output checks its originating socket before rendering and before publishing
state. Each fresh attach resets once; resize sends the existing controller
command without resetting the terminal. Closing or releasing during discovery
cancels the eventual attachment. Initial worker reconnection reuses the acquired
client/generation and never queues keys. Established disconnection freezes input;
explicit Reconnect creates a fresh attachment to the same terminal, replaying the
worker's current screen. Closing removes attachment/repaint/retry timers. A socket
or repaint that never announces readiness becomes an explicit failure after five
seconds. A ready stream with no bytes shows “waiting for terminal output”. Compact
panes show connection/empty/failure status in the existing toolbar.

Server and worker authorization are preserved: enrolled-node ownership and live
attempt checks, same-origin owner authentication, current controller client and
generation, connection-bound output and input, and controller-scoped resize.
When control is unavailable to the UI, the live viewer uses `read_only=true` and
sends no input, Escape or resize. The existing resize implementation documented in
`docs/operations/terminal-resize.md` is reused. No task lifecycle, review gate,
worker source, host service, credential or budget configuration changes.

## Bundle delivery

`check-dashboard-candidate.py` verifies the published archive checksum, release
record, manifest and every asset before applying the local repair. The patch
fails closed if the pinned function/markup anchors change. All JavaScript/CSS
assets and their references receive a repair-specific cache namespace, avoiding
reuse of immutable URLs for changed bytes. The emitted manifest recalculates
asset sizes/digests and declares `dirty=true, official=false`, retaining the
upstream source revision. The command reports both upstream and emitted manifest
digests. Docker's dashboard-build stage executes the same verifier/extractor;
the runtime copies only its resulting assets. Python is confined to that build
stage. The official upstream candidate JSON remains an upstream release record,
not a claim that the emitted local bundle is an official dashboard release.

This bounded overlay is specific to the pinned release and should be replaced
when a reviewed dashboard release contains the repair. It requires no npm package
download, sibling checkout or external source download to build or test.

## Verification

- `node --test scripts/test-dashboard-terminal.mjs`: 14 passed. Includes original
  artifact reproduction, initial paint and subsequent output, correct attempt
  routing/client/generation, input and resize without clearing, read-only refusal,
  finite transport preservation, binding mismatch, unavailable/empty/timed-out
  states, agent switch/return, initial retry, established disconnect/reconnect,
  and close/release while discovery is pending. Also syntax-checks the complete
  emitted InteractiveTerminal module.
- `PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-dashboard-candidate.py`: 7 passed.
  Includes upstream integrity refusals, extraction, local provenance, deterministic
  output, reference rewriting, new cache names after a repair change and refusal
  of a mismatched patch anchor.
- `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check-dashboard-candidate.py`: passed;
  89 assets verified and repaired. This is the extraction path wired into Docker.

- `export CARGO_TARGET_DIR=/tmp/aq-target; cargo test -p aq-manual-sessions --lib stream:: --locked`:
  5 passed, covering ready geometry/connection binding, repaint input gating,
  controller-only resize, dimension bounds and the binary input budget.

- `cargo fmt --all --check`: passed.

While strengthening the reproduction to execute the original control adapter too,
its first run failed because the test context lacked the unchanged identity
helper. The fixture was corrected; the final run passed all 14 cases.

The JavaScript regression command is included in the existing dashboard CI check.

## End-to-end environment and remaining evidence

No end-to-end running-task terminal check was possible: `DATABASE_URL` is unset
and this sandbox has no Docker, PostgreSQL client or browser executable. The
socket/worker portions of the JavaScript tests are deterministic doubles, not a
real tmux task, owner login or rendered browser. Neither a Docker image build nor
production symptom reproduction was performed. Upstream TypeScript typecheck,
lint and rebuild were not run; the repair is untranspiled local JavaScript,
syntax-checked and exercised in the emitted bundle. The existing database-backed
`queued_attempt_terminal_uses_the_live_stream_of_its_own_running_node` integration
case remains the authorization/connection/resize coverage for required CI.

Owner/CI follow-up verification: select a real running task's agent in Agents,
confirm its initial screen and subsequent output, type into the same agent using
the owner's supported control, resize without losing the screen, switch between
two agents, navigate away/back, disconnect/reconnect, and inspect unavailable and
empty states. Check both desktop and phone rendering. Upstream Agents design
instructions and the named reference layouts were read; actual visual acceptance
is unverified here. Offline client checks cannot replace this running-task check.

The workspace has no Git metadata or origin/main history, so integrating a newer
main, creating a PR or running the comparative build budget is unavailable here.
The runner retains these changes for review; nothing was committed, pushed,
merged or deployed. `build-budget.toml` was not edited.
