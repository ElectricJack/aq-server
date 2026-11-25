# Running task terminal browser pilot

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
migrations. In a second terminal, stage a private aq-git socket and two local
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
cannot supply the exact pinned object. These bundles demonstrate the Git substrate and are separate from
the task's delivery evidence. Ctrl-C stops
its daemon. The two seed repositories and broker logs remain available for
local inspection in that directory.
