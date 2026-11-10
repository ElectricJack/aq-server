# Manual interactive agents

**Owner requirements:** manual launch chooses a project without an assignment;
the agent stays interactive until stopped. Launch has no modal. Pending and
startup progress appear as agent nodes in the Swarm graph. Worker slots reflect
the selected project and can be added/removed from the selected worker pane.
Remove terminal watch/edit mode controls and automatically connect for input.
Investigate repeated terminal disconnects, including idle sessions/reconnects.

**Decision (my call):** give manual sessions a separate retained lifecycle rather
than pretending that an idle interactive agent is an unfinished coding task.
The existing finite, qualified task runner and its completion/report semantics
continue serving automated tasks. Manual sessions use the installed subscription,
project source and sandbox, with a persistent tmux session inside the sandbox.
No inference or task text is needed to reach the interactive prompt.

## Implementation

1. Add a bounded manual-session service crate and migration: idempotent owner
   start/stop, per-project worker slot limits, node-authenticated claim/progress,
   renewable liveness, and bounded disposable terminal output/input delivery.
   Restrict every read/write to its project and original worker. Revalidate owner,
   enrollment and enabled project; credentials never enter the sandbox.
2. Add a worker manual supervisor alongside the existing service. Prepare an
   isolated source checkout, upload it into the existing fixed-policy sandbox,
   and start Claude interactively under tmux. No `--print` or initial assignment.
   Keep source/sandbox on stop or failure. Renew a short liveness watchdog while
   the worker is healthy; browser disconnects do not terminate an agent. Restore
   control of the same session after a transport interruption. Pause native polls
   where necessary to prevent a manual/native slot race.
3. Use the original worker node identities. Advertise the actual manual capability
   and limits. Per-project limits are operator settings within the worker's hard
   bound; removing occupied capacity drains it without killing agents.
4. Replace WorkerLaunch's dialog/task composer with inline project/launch controls.
   Retain uncertain launch identity and show its placeholder immediately in the
   graph. Merge retained server progress until running, stopped or failed; never
   infer ready from launch acceptance. Keep selection stable through startup.
5. Open manual tmux output/input in the existing terminal composition. Remove mode
   dropdowns and Take/Release control UI across Agents. Automatically acquire and
   renew control for supported terminals, including reconnects to the same epoch.
   Never replay uncertain keystrokes or enable input without current authority.

## Verification and rollout

- Service tests: owner/node/project isolation, idempotency, slot bounds/draining,
  ordered input and uncertain delivery, bounded output, stop and lease expiration.
- Worker tests: actual tmux lifecycle, no inference on startup, input/resize,
  pause/reconnect and preserved source; use the existing test targets.
- UI tests: inline launch and duplicate/uncertain submit, immediate placeholder,
  progress/ready transition, project slot changes, default input and reconnect.
- Live acceptance on isolated aq-ms1: launch aq-server without a prompt, connect,
  type a brief check, observe two turns in the same session, disconnect/reconnect,
  and explicitly stop with source retained. Review desktop/phone captures against
  Swarm reference 14 and terminal references 08/11, with owner-requested mode UI
  changes taking precedence. Preserve the installed official bundle manifest.
- Private targets per worktree; focused tests under timeout; line ratchets only
  decrease, no new integration binaries. Draft PRs remain unmerged for review.
- Back up scoped config/database and prior bundle before promotion. Touch only
  aq-ms1 services. Leave v1, Supabase, the owner's local worker and global pools
  alone. Document remaining limitations and actual evidence without claiming
  that a queued intent or a historical capture proves a running terminal.
