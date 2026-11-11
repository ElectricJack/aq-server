# Worker host terminal

Owner request, November 11, 2026: open a terminal on any worker machine,
including an enrolled WSL laptop with no SSH listener. AQ1's host-shell behavior
is the reference: a separate login shell, never an agent terminal.

Swarm's selected worker offers **Open terminal**. It opens or resumes that
node's single host shell, with the node name, Linux/WSL host and service-user
authority visible. Closing the view detaches; **End shell** terminates only
that shell. Terminal typing, resize, reconnect and phone keys use the existing
terminal renderer. Output is not archived as an agent transcript.

The enrolled node makes an outbound authenticated connection. Browser access
requires owner authority, mutation protection and an allowed WebSocket origin.
The shared byte transport retains its controller/connection fences, bounds and
no-input-replay behavior. A separate host-shell runtime and database table keep
host shells out of agent lists, quotas, queues and agent Stop.

A separately installed companion owns host tmux sessions on a private socket.
Shells start in the service user's home with a small environment allowlist;
worker credentials and daemon environment are not inherited. The companion
rechecks desired state and credential authorization. Revocation or prolonged
loss of authorization ends its host shells. Browser disconnect alone does not.
An exited shell is reported ended and is never silently respawned.

Existing installations receive the companion through the checksum-pinned
control receiver update. It downloads and verifies the immutable bundle,
extracts only the companion executable and tmux dependencies, and starts a
separate user service. It does not replace or restart the main worker or any
agent. Readiness comes from an actual companion heartbeat; offline/older nodes
show an actionable unavailable state.

Qualification covers owner/node/origin boundaries, stale controllers, independent
manual-agent operation, real PTY input/resize, detach/reconnect/end, exited shells,
receiver bundle validation and desktop/phone browser evidence. Rollout must
preserve existing user agents and keep the AQ1 and legacy worker services intact.

Delivered November 11: see [rollout and qualification](../handoff/2026-11-11-agents-terminal-workspace.md#worker-host-terminal-follow-up-deployed).
