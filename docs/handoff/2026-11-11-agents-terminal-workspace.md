# November 11: Agents terminal workspace integration

The November 9–10 Codex conversation survived in local history under
`01a1c24f-8051-71e0-8755-30fe44fc2cb9` (Review agent-queue rewrite next).
Resume with `codex resume 01a1c24f-8051-71e0-8755-30fe44fc2cb9 -C /home/jkern/dev`.
The November 11 recovery session is `01a1cb25-9237-7403-8b8c-294c6680cb6e`.

The approved [spec 0009](../specs/0009-agents-terminal-workspace.md) is implemented
and integrated in the following worktrees under `/home/jkern/dev/aq-platform/wt`:

| Repository | Worktree | Branch | Qualified implementation |
| --- | --- | --- | --- |
| aq-dashboard | `agents-terminals-dashboard` | `feat/agents-terminal-workspace` | `43552ac` |
| aq-server | `agents-history-server` | `feat/session-transcript-archive` | `a17106c5` |
| aq-worker | `agents-history-worker` | `feat/manual-session-recording` | `29415877` |

Later documentation commits do not change those qualified implementations.
The dashboard includes the final History reader through `b5beff1`; the old
`agents-history-dashboard` branch is superseded and must not be merged again.
The approved spec/assignment branch has also been merged into the server archive
branch. These branches retain their original November 10 prerequisites; review
their baseline before merging them into main or preparing release pins.

Agents is below Supervisor and defaults to a roster/four-terminal workspace.
Swarm and retained History have separate routes. Closing disconnects one pane;
manual Stop ends that agent and retains output/history. Phones/tablets switch
between selected panes without reconnecting; the compact phone layout keeps touch
keys and an exit visible. History supports bounded pages, full retained search,
cross-page match navigation, scoped copy and complete downloads. Old session-list
and graph links preserve their URL scope/selection through compatibility routing.

The worker records before provider startup without a browser attached, uploads
ordered durable chunks independently of capture, and finalizes complete/partial
coverage. Corrupt state releases the sandbox and recovers the server's accepted
sequence to finalize partial output; repeated conflicts stop automatic recovery
and preserve pending chunks. Server archive storage, reads and bounded literal
search enforce original node and owner fences.

Validation completed:

- Full dashboard unit baseline: 126 files / 1,280 tests. Final focused regression
  check: six affected suites / 51 tests. Typecheck, lint and production build pass.
- `pnpm test:workspace`: five production-preview browser tests pass. Desktop,
  tablet, 390px and 320px evidence was inspected; controls are visible and no
  page-level horizontal overflow occurs. All 61 served assets match their bundle
  manifest. Actual browser/xterm is used with labelled HTTP/WebSocket peers.
- Full server `manual_agents::` module: 7 passed, 2 ignored. It includes the real
  WebSocket stream bytes/input fence proof. Both opt-in ignored tests passed
  separately using the actual worker recorder, TCP server router and private SQL.
- Ten focused worker recorder tests pass. A production Docker RUNNER captured
  114,828 bytes / 1,402 lines, and real worker tick/ServerLink uploaded/finalized it;
  a fresh admin router retrieved/searched beginning, middle and final output.
  Real-server corrupt-state recovery retained two accepted chunks and finalized
  partial with the expected reason. The separate 128MiB raw cap did not stop the
  disposable agent. Rust formatting/build budgets and scoped server, worker-binary
  and node-service Clippy pass with warnings denied.

Repeat the combined server tests in `it_server` with a private `DATABASE_URL`,
`AQ_RECORDING_WORKER_DIR` pointing to the worker worktree, and Docker fixture image
`aq-bootstrap-tmux-proof:26.04` available:

```sh
timeout 900 cargo test -p aq-server --test it_server manual_agents::
timeout 900 cargo test -p aq-server --test it_server manual_agents::retained_transcript_real_worker_ -- --ignored
```

The second command starts worker tests from the specified sibling checkout and
passes private temporary credentials by file, not command arguments. Use each
worktree's own target directory. Dispose only the explicitly created test database
and containers. The November 11 fixtures were removed after the checks.

The dashboard's `docs/implementation/agents-terminal-workspace.md` and
`docs/implementation/evidence/agents-terminal-workspace-2026-11-11/` hold detailed
results, screenshots and the served bundle manifest (clean source `43552ac`).

Remaining delivery boundaries: durable capture covers newly launched manual
Claude/Codex agents; old sessions cannot acquire missing earlier output. Native
queue sessions retain their existing inspection/terminal surfaces, with no new
native recorder or native Stop claim. Real phone IME and a full database restart are not separately qualified.
Actual live server-container recreation retained the finalized transcript. No provider inference was needed in the
disposable qualification.

## Owner rollout, November 11

The owner authorized rollout. Server source `7ce7ed7e`, schema 113 and dashboard
`ab4c6bc4` were activated on aq-ms1. The desktop worker is `a61d2ad8`; the hosted
installer was refreshed with that runtime. A real rollout exposed a created-
container startup race, fixed and qualified in worker commits `63127540` and
`a61d2ad8` before the final runtime activation.

Actual browser/worker checks passed on desktop and 390px/320px widths: isolated
input, stopping the disposable session, retained search and complete download.
A second disposable session captured output with no browser attached. Its bytes
and metadata survived actual server-container recreation. Forced-stop archives
correctly report partial coverage. No provider inference was submitted.

A reboot-stopped source registry was restarted with its existing volume and a
restart policy. The preexisting Git recovery fence was cleared using its supported
audited recovery command after stopping and inspecting the service; no refs were
changed. Server, database and Git are healthy; graph and Git readiness are ready.

The owner's two active desktop/laptop agents were preserved. The laptop remains
on its previous main worker runtime: its controls receiver cannot replace that
runtime, and SSH was not configured. The owner subsequently requested a worker
host terminal; spec 0010 records that follow-up. AQ1, the legacy local worker,
Supabase and main branches were untouched.

Private rollback snapshots and exact live receipts are under
`~/.local/share/aq-ms1/agents-workspace-rollout-20261111T154620Z/`.
The database snapshot contains private credentials and must not be published.
Live browser evidence remains local at
`/home/jkern/dev/aq-platform/qa/2026-11-11/agents-workspace-rollout/`.
Schema 113 is newer than the previous server binary supports; rollback of the
binary alone is unsafe. Preserve subsequent writes and fix forward.
