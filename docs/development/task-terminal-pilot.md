# Running task terminal browser pilot

Run this from the server pilot worktree. The build uses the existing dashboard integration binary and the worktree's own Cargo target. The runner needs Docker with the local `postgres:17-bookworm` image, tmux, and Python with `websockets` 16.

```sh
cargo test --locked -j 3 -p aq-server --test it_dashboard --no-run --message-format=json > /tmp/aq-terminal-build.jsonl
pilot_binary=$(python3 -c 'import json, sys; print(next(r["executable"] for r in map(json.loads, sys.stdin) if r.get("target", {}).get("name") == "it_dashboard" and r.get("executable")))' < /tmp/aq-terminal-build.jsonl)
python3 scripts/task-terminal-pilot.py /path/to/verified/dashboard/dist --test-binary "$pilot_binary"
```

The runner creates a new PostgreSQL instance with an independent password, installs the real Store schema, and records a fixture-seeded native attempt as running. It prints the loopback origin and task, session, attempt, and terminal IDs after the worker stream authenticates. The machine credential and terminal encryption key are confined to the private pilot directory.

Open `/agents/{session_id}` at the emitted origin and select Terminal. Verify the current session resolves to the reported attempt and shows `AQ_TERMINAL_PILOT_READY`. Type `printf '\101Q_PILOT_TYPED\n'` followed by Enter and check for `AQ_PILOT_TYPED` in the actual terminal output. The octal escape lets the test distinguish new shell output from the typed command. For disconnected output, inject another escaped marker with `tmux -S "$pilot_directory/tmux" send-keys -l "$command"` followed by `tmux -S "$pilot_directory/tmux" send-keys Enter`. Reconnect in the browser and verify the marker remains visible. Check the worker stream or browser request log for one control acquisition per connection generation.

From the dashboard worktree with the browser test, run the assertions against
the listener and exact pane:

```sh
AQ_LIVE_URL="$origin" AQ_DISPOSABLE_TERMINAL_SESSION="$session_id" \
  AQ_DISPOSABLE_TERMINAL_SOCKET="$pilot_directory/tmux" pnpm test:live running-task-terminal
```

Stop the runner with Ctrl-C. It stops its test server, tmux session, and both disposable containers. Run again with each immutable dashboard bundle to compare builds. The tmux shell starts in an unprivileged container with no network or host mounts. This exercises the actual HTTP routes, PostgreSQL authority, WebSocket transport, browser controller, and PTY. Task admission and remote worker conditions come from the shared native fixture. The pilot does not run a provider or publish branches or PRs.
