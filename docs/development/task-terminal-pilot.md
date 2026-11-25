# Running task terminal browser pilot

## Terminal repair qualification, 2026-11-25

The rehearsal used the ID and title of the original terminal ticket,
`a3c72f06-d1a6-428e-bf67-b0f1c734a8c9`, in a private Graph workspace.
The earlier repair is retained at `3bed9a998cd64766f0cdcd0f0da636af21f66da3`.
Its fourteen terminal regressions, seven dashboard package checks and
database-backed WebSocket regression passed locally.

| Test | Observed result |
| --- | --- |
| Resource request on schema 183 | A running attempt requested a writable sibling and PostgreSQL together. Two machine polls delivered the same checkpoint. Target review rejected a stale commit and approved the pinned one. |
| Fresh checkout after a branch moves | A fresh Git receiver restored the approved commit after its branch disappeared. The authenticated server route replayed identical bundle bytes and refused access after revocation. |
| Browser and terminal | Dashboard v0.1.6 acquired control twice for one socket. Dashboard `1a2e343` acquired control once, accepted shell input, replayed output written during disconnect, and accepted input after reconnect. Chromium ran against the resource-seeded server and a real tmux shell. |
| Files and SQL together | An isolated Docker Engine container with UID 10002 ran SQL over a private socket and returned the output in its writable sibling archive. The kernel rejected a write to its read-only sibling. |

The browser used an authenticated worker protocol peer. The physical SQL test
used a rootful Docker daemon. The native worker control, archive and scheduler
paths have separate tests. This host lacks the UID/GID mapping helpers for a
physical multi-UID rootless worker rehearsal.

## Run the pilot

Run this from the server pilot worktree. The build uses the existing dashboard integration binary and the worktree's own Cargo target. The runner needs Docker with the local `postgres:17-bookworm` and Graph images, tmux, and Python with `websockets` 16.
The runner defaults to the pinned local Graph image. Supply another existing fixture image with `--graph-image`.

```sh
cargo test --locked -j 3 -p aq-server --test it_dashboard --no-run --message-format=json > /tmp/aq-terminal-build.jsonl
pilot_binary=$(python3 -c 'import json, sys; print(next(r["executable"] for r in map(json.loads, sys.stdin) if r.get("target", {}).get("name") == "it_dashboard" and r.get("executable")))' < /tmp/aq-terminal-build.jsonl)
python3 scripts/task-terminal-pilot.py /path/to/verified/dashboard/dist --test-binary "$pilot_binary"
```

The runner creates a new PostgreSQL instance with an independent password, installs the real Store schema, seeds the retained ticket title and UUID into a new Graph workspace, and records a fixture-seeded native attempt as running. It prints the loopback origin and task, session, attempt, and terminal IDs after the worker stream authenticates. The machine credential and terminal encryption key are confined to the private pilot directory.

Open `/agents/{session_id}` at the emitted origin and select Terminal. Verify the current session resolves to the reported attempt and shows `AQ_TERMINAL_PILOT_READY`. Type `printf '\101Q_PILOT_TYPED\n'` followed by Enter and check for `AQ_PILOT_TYPED` in the actual terminal output. The octal escape lets the test distinguish new shell output from the typed command. For disconnected output, inject another escaped marker with `tmux -S "$pilot_directory/tmux" send-keys -l "$command"` followed by `tmux -S "$pilot_directory/tmux" send-keys Enter`. Reconnect in the browser and verify the marker remains visible. Check the worker stream or browser request log for one control acquisition per connection generation.

From the dashboard worktree with the browser test, run the assertions against
the listener and exact pane:

```sh
AQ_LIVE_URL="$origin" AQ_DISPOSABLE_TERMINAL_SESSION="$session_id" \
  AQ_DISPOSABLE_TERMINAL_SOCKET="$pilot_directory/tmux" pnpm test:live running-task-terminal
```

Stop the runner with Ctrl-C. It stops its test server, tmux session, and the disposable containers, and their dedicated bridge. Run again with each immutable dashboard bundle to compare builds. The tmux shell starts in an unprivileged container with no network or host mounts. This exercises the actual HTTP routes, PostgreSQL authority, WebSocket transport, browser controller, and PTY. Task admission and remote worker conditions come from the shared native fixture. The pilot does not run a provider or publish branches or PRs.


## Workspace request and review

Use a server source revision containing the workspace resource and checkpoint
migrations. Add `--features delivery_playbook,project_environments,supervisor_full_access`
to the build command and use the executable from that build. In a second terminal, stage a private aq-git socket and two local
repository identities.

```sh
workspace_git=$(mktemp -d /tmp/aq-task-workspace-git.XXXXXX)
python3 scripts/task-workspace-pilot-git.py "$workspace_git"
```

Add `--workspace-git "$workspace_git/repositories.json"` to the runner.
Add `--workspace-only` to exit after the endpoint assertions and bridge readiness.
The optional phase issues a current attempt credential through the actual worker
API, requests sibling write access and PostgreSQL in one batch, and verifies
the exact canonical commit, a durable checkpoint intent, and redelivery in the
authenticated machine control response. It checks a stale
approval fails, confirms the current target owner approval succeeds, and
reads the final resource state through the owner API. The phase stages the
native launch binding and target policy in the fixture. It does not attest
worker eviction, successor hydration, forge PR creation, or task results.

The aq-git stage fetches only `file:` sources created under its own private
directory. It exports one checkpoint bundle per repository, verifies the bundle bytes,
restores each into an empty bare receiver, and
prints the exact base and head objects, logical repository IDs, and retention
handles. The sibling pin is on an orphan branch, so its default branch history
cannot supply the exact pinned object. The stage removes that branch from its
disposable remote after retaining it, proves a fresh fetch misses it, and
recovers the exact tree from an aq-git export of the active retention handle.
These bundles demonstrate the Git substrate and are separate from
the task's delivery evidence. Ctrl-C stops
its daemon. The two seed repositories and broker logs remain available for
local inspection in that directory.
