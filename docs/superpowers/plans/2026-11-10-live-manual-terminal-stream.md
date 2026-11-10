# Live manual terminal stream

**Owner:** the manual terminal is too slow and its input cursor does not align
with the console. Restore the responsive interaction that v1 provided.

**Finding:** read-only inspection of v1's terminal_stream.py and terminal_pty.py
shows a disposable tmux PTY attachment with bounded raw WebSocket bytes. The v2
manual adapter instead waits for a worker cycle and an input receipt, starts
separate Podman commands, captures tmux history, and resets xterm on each screen.
The dedicated no-inference baseline took 15330 ms to show an 18-character string.
Captures omit live cursor/mode state, so re-rendering them is not PTY parity.

**My call:** keep manual lifecycle and project/worker authorization, and replace
the normal terminal transport with a disposable raw tmux attachment. Use bounded
WebSockets browser→server→original enrolled worker, validate controller generation
and per-attachment identity, and recheck owner/node/project liveness. Never replay
keyboard bytes after a transport loss. Retain legacy HTTP routes for compatibility;
finite qualified task terminals are not changed.

The worker's scoped `terminal-relay --service-config` companion owns only attach
clients. It reads the installed profile and original enrollment, uses one long-lived
Podman exec PTY, streams cursor/terminal escapes unchanged, writes input directly,
and propagates actual PTY dimensions. Deploying/restarting this companion must not
stop or relaunch existing agents. No credential enters a sandbox or URL.

Verification: real-SQL/production-router WebSockets for original node/origin,
ordered unacknowledged input, raw cursor bytes, resize and takeover fencing; real
local PTY test; browser transport/backpressure/no-replay tests; live no-inference
keyboard/Unicode/edit/resize/reconnect checks on a separate QA agent; before/after
latency and cursor screenshots. Preserve desktop/phone layout, inspect screenshots,
verify the official served bundle, retain private build targets and line budgets.
